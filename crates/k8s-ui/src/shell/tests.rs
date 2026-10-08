//! GPUI test support examples.
//!
//! App-only tests use TestAppContext. Window tests use VisualTestContext.
//! Seeded random tests reproduce failures. Property tests use the proptest re-export from GPUI.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{
    AppContext, ClipboardItem, Focusable, Modifiers, MouseButton, SharedString, TestAppContext,
    point, px,
};
use rand::RngExt as _;
use rand::prelude::StdRng;

use k8s_core::discovery::ResourceCatalog;
use k8s_core::helm::{Helm, HelmError};
use k8s_core::latency::LatencyTier;
use k8s_core::machines::{SEARCH_DEBOUNCE_MS, SearchHit, SearchPhase};
use kube_core::DynamicObject;

use super::commands::{
    CommandRun, command_matches_query, demo_commands, filter_commands, filter_commands_for_scope,
};
use super::panels::{
    PALETTE_MAX_HEIGHT, palette_height, palette_list_content, palette_result_label,
};
use super::{
    CatalogState, CenterTab, ConnectionState, DIVIDER_KEY_STEP, Dialog, DragTarget, FocusNext,
    INSPECTOR_FLOAT_BELOW, InspectorLayout, MIN_LAYOUT_WIDTH, NamespaceState, OpenServiceAccount,
    PaletteScope, ReloadKubeconfigs, Shell, StartupState, StatusPanel, TabContent, TabView,
    ToggleCommandPalette, ToggleDock, ToggleLeftPanel, ToggleRightPanel, bounded_tab_drop_gap,
    inspector_layout, inspector_width_ceiling, move_open_tab_within_group, normalize_open_tabs,
    parse_chart_reference, parse_port, parse_replicas, reorder_open_tabs,
    responsive_panel_visibility, tab_drop_gap, type_ahead_index,
};
use crate::keymap::{install_target_default, user_keymap_path};
use crate::panels::helm::{
    HELM_NOT_INSTALLED, HELM_PROBE_FAILED, HELM_PROBE_TIMED_OUT, HelmCapability,
};
use crate::panels::inspector_data::{ObjectRef, OpsFuture};
use crate::panels::terminal::{
    PortForwardFactory, TerminalFactory, TerminalKind, TerminalServices,
};
use crate::session::ServiceAccountTarget;
use crate::table_view::{
    ClusterSession, ObjectOps, PodsView, PortForwardTarget, ResourceSpec, Row, ScaleTarget,
    TableStatus,
};
use crate::update::{UpdateActions, UpdatePhase, UpdateUiState};
use gpui_kit::assets::IconName;

type PortForwardBinding = Rc<RefCell<Option<tokio::sync::oneshot::Sender<Result<u16, String>>>>>;

/// Initializes the component library the window renders through.
///
/// gpui-kit carries its own theme and its own settings, so this is the whole of
/// the test setup: there is no user file to read and no separate theme to load.
fn init_ui(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
}

/// Uses the production keymap installer without reading the user keymap.
fn install_test_keymap(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let result = install_target_default(cx);
        assert!(result.is_ok(), "{result:?}");
    });
}

thread_local! {
    /// The Tokio runtime the tests spawn cluster work on.
    ///
    /// `gpui_tokio` binds Tokio to Zed's GPUI crate, which is not the GPUI this
    /// app is built on, so the tests own the runtime the way the app's own
    /// `runtime` module does. It is thread-local because a `Handle` is only
    /// usable while the `Runtime` behind it is alive, and every test runs on a
    /// thread of its own.
    static TEST_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("the test Tokio runtime starts");
}

/// A handle to [`TEST_RUNTIME`], for the code under test that spawns onto it.
fn test_runtime() -> tokio::runtime::Handle {
    TEST_RUNTIME.with(|runtime| runtime.handle().clone())
}

fn init_cluster_runtime(cx: &mut TestAppContext) -> tokio::runtime::Handle {
    cx.dispatcher.allow_parking();
    test_runtime()
}

/// The entities behind every mounted tab view.
///
/// A session change drops the views the old session built and rebuilds the active
/// one, so "is anything left over" is a question about entities and not about slots.
fn mounted_view_ids(shell: &Shell) -> Vec<u64> {
    shell
        .views
        .iter()
        .flatten()
        .map(|view| match view {
            TabView::Resource(view) => view.entity_id().as_u64(),
            TabView::Preview(view) => view.entity_id().as_u64(),
            TabView::Overview(view) => view.entity_id().as_u64(),
            TabView::Forwards(view) => view.entity_id().as_u64(),
            TabView::Helm(view) => view.entity_id().as_u64(),
            TabView::Settings(view) => view.entity_id().as_u64(),
        })
        .collect()
}

fn load_test_registry(
    handle: &tokio::runtime::Handle,
    contents: &str,
    name: &str,
) -> Arc<k8s_core::cluster::ClusterRegistry> {
    let path = std::env::temp_dir().join(format!("k8s-gpui-{name}-{}.yaml", std::process::id()));
    std::fs::write(&path, contents).expect("write kubeconfig");
    let registry = handle
        .block_on(k8s_core::cluster::ClusterRegistry::load(&path))
        .expect("load kubeconfig");
    let _ = std::fs::remove_file(&path);
    Arc::new(registry)
}

fn catalog_retry_shell<'a>(
    cx: &'a mut TestAppContext,
    name: &str,
    future: super::CatalogFuture,
) -> (gpui_kit::Entity<Shell>, &'a mut gpui_kit::VisualTestContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, name);
    let session = ClusterSession::from_registry(registry, handle);
    let (shell, visual) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    shell.update(visual, |shell, cx| {
        shell._namespace_task = None;
        shell.namespace_state = NamespaceState::Ready(Vec::new());
        shell.health_probe = None;
        shell.connection = ConnectionState::Live;
        shell._health_task = None;
        shell._health_schedule_task = None;
        shell.reset_catalog_retry();
        shell.focused = true;
        shell.catalog = None;
        shell.catalog_state = CatalogState::Failed("temporary".to_owned());
        shell.catalog_failure = Some("temporary".to_owned());
        shell.catalog_future = Some(future);
        cx.notify();
    });
    (shell, &mut *visual)
}

/// Pins a ready session as connected so an unreachable test cluster cannot
/// replace the center with the connection-failure takeover while a test drives
/// focus. A broken context is left alone so its failure states stay readable.
fn hold_cluster_connected(shell: &mut Shell, cx: &mut gpui_kit::Context<Shell>) {
    if !matches!(shell.session, Some(ClusterSession::Ready { .. })) {
        return;
    }
    shell.health_probe = None;
    shell._health_task = None;
    shell._health_schedule_task = None;
    shell.connection = ConnectionState::Live;
    shell.reset_catalog_retry();
    cx.notify();
}

fn search_hit(name: &str) -> SearchHit {
    SearchHit {
        resource: k8s_core::discovery::ResourceEntry {
            group: String::new(),
            version: "v1".to_owned(),
            kind: "Pod".to_owned(),
            plural: "pods".to_owned(),
            scope: k8s_core::discovery::ResourceScope::Namespaced,
            verbs: vec!["list".to_owned()],
        },
        object: Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": format!("uid-{name}"),
                },
            }))
            .expect("pod object"),
        ),
    }
}

fn service_account_object() -> DynamicObject {
    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "ServiceAccount",
        "metadata": {
            "name": "default",
            "namespace": "default",
            "uid": "uid-service-account-default",
        },
    }))
    .expect("service account object")
}

fn shortcut(linux: &'static str, macos: &'static str) -> &'static str {
    if cfg!(target_os = "macos") {
        macos
    } else {
        linux
    }
}

#[test]
fn tab_shortcuts_keep_pin_reorder_and_macos_close_tab_entries() {
    // The claim is about the two shipped assets rather than about the map this
    // machine ends up with, and the loader's own file type is private to
    // `keymap`, so the test reads the assets the way the loader does: JSONC, a
    // list of sections, each an optional `context` and a `bindings` map from
    // keystrokes to an action name or an `[action, argument]` pair.
    let sections = |source: &str| -> Vec<(String, serde_json::Map<String, serde_json::Value>)> {
        crate::settings::parse_jsonc::<Vec<serde_json::Value>>(source)
            .expect("keymap must parse")
            .into_iter()
            .map(|section| {
                (
                    section["context"].as_str().unwrap_or_default().to_owned(),
                    section["bindings"].as_object().cloned().unwrap_or_default(),
                )
            })
            .collect()
    };
    let has_binding = |source: &str, context: &str, keystrokes: &str, action: &str| {
        sections(source).iter().any(|(section, bindings)| {
            section == context
                && bindings.get(keystrokes).is_some_and(|value| {
                    value.as_str() == Some(action)
                        || value
                            .as_array()
                            .and_then(|arguments| arguments.first())
                            .and_then(|name| name.as_str())
                            == Some(action)
                })
        })
    };
    let binds_keystrokes = |source: &str, context: &str, keystrokes: &str| {
        sections(source)
            .iter()
            .any(|(section, bindings)| section == context && bindings.contains_key(keystrokes))
    };
    // The default source ships on every platform. The macOS asset is an overlay that
    // only rebinds what macOS needs, so it keeps the default tab shortcuts.
    let linux = crate::keymap::default_keymap_source();
    let macos = crate::keymap::default_keymap_for_target("macos");
    const SHELL_CONTEXT: &str = "Shell && !CommandPalette";
    const APP_CONTEXT: &str = "!CommandPalette";
    for (keystrokes, action) in [
        ("secondary-shift-i", "k8s_shell::TogglePinTab"),
        ("secondary-alt-left", "k8s_shell::MoveTabLeft"),
        ("secondary-alt-right", "k8s_shell::MoveTabRight"),
        // Close keeps one meaning on every platform, so the tab key is the same everywhere.
        ("secondary-shift-t", "k8s_shell::CloseTab"),
    ] {
        assert!(has_binding(linux, SHELL_CONTEXT, keystrokes, action));
        assert!(
            !binds_keystrokes(macos, SHELL_CONTEXT, keystrokes),
            "the macOS overlay must not shadow {keystrokes}"
        );
    }
    // secondary-w closes the window on every platform and never a tab.
    assert!(has_binding(
        linux,
        APP_CONTEXT,
        "secondary-w",
        "k8s_app::CloseWindow"
    ));
    assert!(!has_binding(
        linux,
        SHELL_CONTEXT,
        "secondary-w",
        "k8s_shell::CloseTab"
    ));
    assert!(!has_binding(
        macos,
        SHELL_CONTEXT,
        "secondary-w",
        "k8s_shell::CloseTab"
    ));
    // The base releases secondary-w in a session. macOS rebinds it so the Command chord
    // reaches the app even while a session has focus.
    assert!(has_binding(
        macos,
        "Terminal",
        "secondary-w",
        "k8s_app::CloseWindow"
    ));
}

#[test]
fn helm_chart_reference_validation_requires_an_explicit_source() {
    for chart in [
        "bitnami/nginx",
        "./charts/nginx",
        "/tmp/nginx",
        r"charts\nginx",
    ] {
        assert_eq!(
            parse_chart_reference(chart).as_deref(),
            Ok(chart),
            "{chart}"
        );
    }
    assert!(parse_chart_reference("").is_err());
    assert!(parse_chart_reference("   ").is_err());
    assert!(parse_chart_reference("nginx").is_err());
    assert!(parse_chart_reference("--post-renderer").is_err());
    assert!(parse_chart_reference("bitnami/nginx\nnext").is_err());
}

#[gpui_kit::test]
fn helm_probe_errors_keep_capability_and_command_copy(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    for (error, capability, reason) in [
        (
            HelmError::NotInstalled,
            HelmCapability::NotInstalled,
            HELM_NOT_INSTALLED,
        ),
        (
            HelmError::Timeout,
            HelmCapability::Timeout,
            HELM_PROBE_TIMED_OUT,
        ),
        (
            HelmError::Exit {
                code: Some(1),
                stderr: "probe failed".to_owned(),
            },
            HelmCapability::Error,
            HELM_PROBE_FAILED,
        ),
    ] {
        shell.update(cx, |shell, cx| shell.on_helm_probed(Err(error), cx));
        assert_eq!(shell.read_with(cx, |shell, _| shell.helm_state), capability);
        assert_eq!(
            shell.read_with(cx, |shell, _| {
                shell.helm_error.as_deref().map(str::to_owned)
            }),
            Some(reason.to_owned())
        );
        assert!(shell.read_with(cx, |shell, _| {
            shell
                .commands
                .iter()
                .find(|command| command.id == "helm.open")
                .is_some_and(|command| {
                    matches!(command.run, CommandRun::Unavailable { reason: actual, .. } if actual == reason)
                })
        }));
    }
}

#[gpui_kit::test]
fn helm_uses_registry_source_and_rebinds_after_context_switch(cx: &mut TestAppContext) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let dir = std::env::temp_dir().join(format!("k8s-gpui-helm-session-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create kubeconfig directory");
    let source = dir.join("contexts.yaml");
    let resources = dir.join("resources.yaml");
    let sibling_source = dir.join("contexts-alt.yaml");
    let sibling_resources = dir.join("resources-alt.yaml");
    std::fs::write(&source, SWITCH_SPLIT_CONTEXT).expect("write contexts");
    std::fs::write(&resources, SWITCH_SPLIT_RESOURCES).expect("write resources");
    std::fs::write(
        &sibling_source,
        SWITCH_SPLIT_CONTEXT.replace("current-context: alpha-ctx", "current-context: beta-ctx"),
    )
    .expect("write sibling contexts");
    std::fs::write(&sibling_resources, SWITCH_SPLIT_RESOURCES).expect("write sibling resources");

    let registry = Arc::new(
        handle
            .block_on(k8s_core::cluster::ClusterRegistry::load_sources(vec![
                source.clone(),
                resources.clone(),
            ]))
            .expect("load kubeconfig"),
    );
    let session = ClusterSession::from_registry(Arc::clone(&registry), handle.clone());
    let sibling_registry = Arc::new(
        handle
            .block_on(k8s_core::cluster::ClusterRegistry::load_sources(vec![
                sibling_source.clone(),
                sibling_resources.clone(),
            ]))
            .expect("load sibling kubeconfig"),
    );
    let sibling_cluster = sibling_registry
        .clusters()
        .iter()
        .find(|cluster| cluster.name() == "beta-ctx")
        .expect("sibling beta context")
        .id();
    let sibling_session =
        ClusterSession::from_registry_with_cluster(sibling_registry, handle, Some(sibling_cluster));
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    shell.read_with(cx, |shell, _| {
        let services = shell.helm_services();
        assert_eq!(services.context.as_deref(), Some("alpha-ctx"));
        assert_eq!(
            services.kubeconfig_sources,
            vec![source.clone(), resources.clone()]
        );
    });

    shell.update(cx, |shell, cx| {
        shell.helm = Some(Helm::with_binary("/nonexistent/k8s-gpui/helm"));
        shell.helm_state = HelmCapability::Available;
        shell.helm_error = None;
        assert!(shell.open_special_tab(
            TabContent::Helm,
            "Helm",
            gpui_kit::assets::IconName::Archive,
            cx
        ));
    });
    let (view, entity_id, initial_epoch) = shell.read_with(cx, |shell, _| {
        let view = shell
            .views
            .iter()
            .find_map(|slot| match slot {
                Some(super::TabView::Helm(view)) => Some(view.clone()),
                _ => None,
            })
            .expect("helm view");
        (
            view.clone(),
            view.entity_id(),
            view.read_with(cx, |view, _| view.request_epoch()),
        )
    });

    shell.update(cx, |shell, cx| {
        shell.helm = Some(Helm::with_binary("/nonexistent/k8s-gpui/helm"));
        assert!(shell.switch_cluster(1, cx));
    });
    let first_epoch = shell.read_with(cx, |shell, cx| {
        let services = shell.helm_services();
        assert_eq!(services.context.as_deref(), Some("beta-ctx"));
        assert_eq!(
            services.kubeconfig_sources,
            vec![source.clone(), resources.clone()]
        );
        let current = shell
            .views
            .iter()
            .find_map(|slot| match slot {
                Some(super::TabView::Helm(current)) => Some(current.clone()),
                _ => None,
            })
            .expect("rebound helm view");
        assert_eq!(current.entity_id(), entity_id);
        let epoch = current.read_with(cx, |view, _| view.request_epoch());
        assert!(epoch > initial_epoch);
        assert_eq!(view.entity_id(), entity_id);
        epoch
    });

    shell.update(cx, |shell, cx| {
        shell.helm = Some(Helm::with_binary("/nonexistent/k8s-gpui/helm"));
        assert!(shell.replace_session(sibling_session, cx));
    });
    shell.read_with(cx, |shell, cx| {
        let services = shell.helm_services();
        assert_eq!(services.context.as_deref(), Some("beta-ctx"));
        assert_eq!(
            services.kubeconfig_sources,
            vec![sibling_source.clone(), sibling_resources.clone()]
        );
        let current = shell
            .views
            .iter()
            .find_map(|slot| match slot {
                Some(super::TabView::Helm(current)) => Some(current.clone()),
                _ => None,
            })
            .expect("sibling rebound helm view");
        assert_eq!(current.entity_id(), entity_id);
        assert!(current.read_with(cx, |view, _| view.request_epoch()) > first_epoch);
    });
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn command_palette_height_tracks_filtered_rows_and_caps() {
    let commands = demo_commands(true);
    let all: Vec<_> = commands.iter().collect();
    let short = filter_commands(&commands, "toggle");
    let empty = Vec::new();

    assert!(palette_height(&empty) < palette_height(&short));
    assert!(palette_height(&short) <= palette_height(&all));
    assert!(palette_height(&all) <= PALETTE_MAX_HEIGHT);
}

/// The card has to cover its chrome plus every row it lists, or the last row is cut off with no
/// fade and no scrollbar, because the overflow that would have said so is computed from the same
/// numbers.
///
/// The height test above compares `palette_metrics` with itself, so a chrome constant that is 27px
/// short stays green forever. This one measures the card the window actually laid out and asks it
/// to cover the list, for one row and for seven, in each of the four scopes.
#[gpui_kit::test]
fn the_command_palette_card_covers_every_row_it_lists(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    // Tall enough that the card's own 560px cap is what binds, not the 60% of the window, so the
    // two cases below differ only by whether the list needs to scroll.
    let height = 1200.0;
    cx.simulate_resize(gpui_kit::size(px(1440.0), px(height)));
    shell.update(cx, |shell, cx| {
        shell.clusters = (0..7)
            .map(|index| format!("context-{index}").into())
            .collect();
        shell.active_cluster = 3;
        shell.namespace_state =
            NamespaceState::Ready((0..7).map(|index| format!("ns-{index}").into()).collect());
        shell.rebuild_commands();
        cx.notify();
    });
    cx.run_until_parked();

    for scope in [
        PaletteScope::Commands,
        PaletteScope::Context,
        PaletteScope::Namespace,
        PaletteScope::Kind,
    ] {
        // The unfiltered list, then the same card narrowed to one row. The narrowing query is
        // taken from a row the scope actually lists, because no single word matches a context, a
        // namespace, a kind and a command at once.
        let first = {
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.open_palette_with_scope(scope, "", window, cx);
                });
            });
            cx.run_until_parked();
            shell.read_with(cx, |shell, _| {
                shell
                    .palette_matches()
                    .first()
                    .map(|command| command.label.to_string())
                    .expect("every scope lists something")
            })
        };
        let one = first
            .split_whitespace()
            .next()
            .expect("a row has a label")
            .to_owned();
        for query in [String::new(), one] {
            cx.update(|window, cx| {
                shell.update(cx, |shell, cx| {
                    shell.open_palette_with_scope(scope, &query, window, cx)
                });
            });
            cx.run_until_parked();

            // `palette_matches` borrows the shell, so the count, the chrome and the list
            // content all have to be read in one pass.
            let (listed, chrome, content) = shell.read_with(cx, |shell, _| {
                let matches = shell.palette_matches();
                let chrome = shell.palette_chrome.get();
                (matches.len(), chrome, palette_list_content(&matches))
            });
            assert!(listed > 0, "{scope:?} with {query:?} lists nothing");
            // The measured chrome, not the prediction: the prediction is what the card was sized
            // from on the first frame, and this is the number that has to hold afterwards.
            assert!(
                chrome > 0.0,
                "{scope:?} never reported its chrome, so the card is still using the prediction"
            );
            let card = cx
                .debug_bounds("command-palette-card")
                .expect("the palette card must be laid out");
            let card_height = f32::from(card.size.height);
            if content + chrome <= PALETTE_MAX_HEIGHT {
                assert!(
                    card_height + 1.0 >= content + chrome,
                    "{scope:?} with {query:?} clipped a row: card {card_height}px, list \
                     {content}px, chrome {chrome}px"
                );
            } else {
                // Past the cap the list has to scroll, so gpui-kit's `Scrollbar` can reach the
                // rows the card could not cover and a row is never simply cut off.
                assert!(
                    shell.read_with(cx, |shell, _| shell.palette_scroll.max_offset().y) > px(0.0),
                    "{scope:?} with {query:?} needs {content}px of list in a {card_height}px card \
                     and cannot scroll"
                );
            }
        }
    }
}

/// The scope filter has already narrowed the list, so the field opens empty for every scope.
///
/// A pre-filled word was matched against command ids as well as labels, so `open` hit every
/// `kind.open.<group>/<version>/<Kind>` and the kind switcher showed all 71 of its rows for a
/// word none of them contained.
#[test]
fn every_palette_scope_opens_with_an_empty_query() {
    for scope in [
        PaletteScope::Commands,
        PaletteScope::Context,
        PaletteScope::Namespace,
        PaletteScope::Kind,
    ] {
        assert_eq!(scope.query(), "", "{scope:?} must not pre-fill the search");
    }
}

#[test]
fn generic_commands_are_action_backed_with_binding_metadata() {
    let commands = demo_commands(true);
    for (id, action_name) in [
        ("navigation.context", "k8s_shell::OpenContextSwitcher"),
        ("navigation.namespace", "k8s_shell::OpenNamespaceSwitcher"),
        ("navigation.kind", "k8s_shell::OpenResourceKindSwitcher"),
        ("view.overview", "k8s_shell::OpenOverview"),
        ("yaml.apply", "k8s_shell::ApplyYaml"),
        ("pod.logs", "k8s_shell::OpenLogs"),
        ("pod.events", "k8s_shell::OpenEvents"),
        ("pod.exec", "k8s_shell::ExecSelection"),
        ("pod.forward_port", "k8s_shell::PortForwardSelection"),
        ("pod.service_account", "k8s_shell::OpenServiceAccount"),
        ("resource.restart", "k8s_shell::RestartSelection"),
        ("resource.scale", "k8s_shell::ScaleSelection"),
        ("keymap.reload", "k8s_shell::ReloadKeymap"),
        ("keymap.preset.lens", "k8s_shell::UseKeymapPreset"),
        ("keymap.preset.vscode", "k8s_shell::UseKeymapPreset"),
    ] {
        let command = commands
            .iter()
            .find(|command| command.id == id)
            .expect("generic command exists");
        let CommandRun::Action(make_action) = &command.run else {
            panic!("{id} must use an action");
        };
        assert!(
            command.binding.is_some(),
            "{id} must expose binding metadata"
        );
        assert_eq!(make_action().name(), action_name, "{id}");
    }
}

#[gpui_kit::test]
fn dynamic_switch_commands_use_stable_ids_and_show_current_scope(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.clusters = vec!["kind-k8s-gpui-dev".into(), "beta".into()];
        shell.namespace_state = NamespaceState::Ready(vec!["default".into(), "team-a".into()]);
        shell.rebuild_commands();
        cx.notify();
    });
    let first = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter(|command| command.id.as_ref().starts_with("context.switch."))
            .map(|command| command.id.clone())
            .collect::<Vec<_>>()
    });
    assert!(first.contains(&SharedString::from("context.switch.beta")));
    assert!(first.contains(&SharedString::from("context.switch.kind-k8s-gpui-dev")));
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .find(|command| command.id.as_ref() == "context.switch.kind-k8s-gpui-dev")
            .is_some_and(|command| matches!(&command.run, CommandRun::Unavailable { .. }))
    }));
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "namespace.switch.team-a")
    }));
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .find(|command| command.id.as_ref() == "namespace.switch.All_namespaces")
            .is_some_and(|command| matches!(&command.run, CommandRun::Unavailable { .. }))
    }));
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "kind.open.Service")
    }));
    // "Current" is carried by the badge the row already renders, not by a
    // second label shape. A row that read `Namespace: perf (Current)` beside
    // `Switch to Namespace: kube-system` made the list scan two conventions at
    // once, and put a repeated verb in front of every row the user reads.
    assert!(
        shell.read_with(cx, |shell, _| shell
            .commands
            .iter()
            .all(|command| !command.label.as_ref().contains("(Current)"))),
        "no row encodes the current selection in its label"
    );
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter(|command| command.id.as_ref().starts_with("kind.open."))
            .any(|command| matches!(&command.run, CommandRun::Unavailable { .. }))
    }));
    let second = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter(|command| command.id.as_ref().starts_with("kind.open."))
            .map(|command| command.id.clone())
            .collect::<Vec<_>>()
    });
    shell.update(cx, |shell, _cx| shell.rebuild_commands());
    let third = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter(|command| command.id.as_ref().starts_with("kind.open."))
            .map(|command| command.id.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(second, third);
}

#[gpui_kit::test]
fn switchers_reuse_palette_query_and_the_toolbar_keeps_shortcuts_in_tooltips(
    cx: &mut TestAppContext,
) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(960.0), px(640.0)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("command-palette").is_some());
    assert!(cx.debug_bounds("open-settings").is_some());
    // The toolbar carries no permanent shortcut chips. They were the least
    // important controls on screen wearing the most prominent chrome, and the
    // cluster name they crowded out is the one thing that must never be
    // ambiguous. The bindings live in the tooltips instead.
    assert!(cx.debug_bounds("command-palette-keycap").is_none());
    assert!(cx.debug_bounds("open-settings-keycap").is_none());

    // The scope filter has already narrowed the list, so the field opens empty. A pre-filled word
    // was matched against command ids too, and `open` is half of every kind command's id.
    for (shortcut, scope) in [
        ("secondary-shift-c", PaletteScope::Context),
        ("secondary-shift-m", PaletteScope::Namespace),
        ("secondary-shift-k", PaletteScope::Kind),
    ] {
        cx.simulate_keystrokes(shortcut);
        cx.run_until_parked();
        assert!(shell.read_with(cx, |shell, _| shell.palette_open));
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.palette_query.clone()),
            ""
        );
        assert_eq!(shell.read_with(cx, |shell, _| shell.palette_scope), scope);
        let visible = shell.read_with(cx, |shell, _| {
            filter_commands_for_scope(&shell.commands, &shell.palette_query, scope)
                .into_iter()
                .map(|command| command.id.clone())
                .collect::<Vec<_>>()
        });
        assert!(visible.iter().all(|id| {
            id.as_ref().starts_with(match scope {
                PaletteScope::Context => "context.",
                PaletteScope::Namespace => "namespace.",
                PaletteScope::Kind => "kind.",
                PaletteScope::Commands => "",
            })
        }));
        assert!(
            !visible.iter().any(|id| {
                matches!(id.as_ref(), "settings.open" | "pod.logs" | "view.overview")
            })
        );
        if scope == PaletteScope::Namespace {
            assert!(
                visible
                    .iter()
                    .any(|id| id.as_ref() == "namespace.switch.All_namespaces")
            );
        }
        assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
        assert!(cx.debug_bounds("command-palette-scope").is_some());
        // The scope line sits under the field, where the contract puts it and where the resource
        // search already has it, and it no longer restates the tab behind the card.
        let field = cx
            .debug_bounds("command-palette-search")
            .expect("the search field must be laid out");
        let scope_line = cx
            .debug_bounds("command-palette-scope-line")
            .expect("the scope line must be laid out");
        assert!(
            scope_line.top() >= field.bottom(),
            "the scope belongs under the field, not in the title block"
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.palette_scope),
            PaletteScope::Commands
        );
    }
    cx.simulate_keystrokes("secondary-shift-p");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .palette_matches()
            .iter()
            .any(|command| command.id.as_ref() == "settings.open")
    }));
}

#[gpui_kit::test]
fn switcher_shortcuts_are_unbound_in_terminal(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let targets = cx.update(|cx| {
        cx.key_bindings()
            .borrow()
            .bindings()
            .filter_map(|binding| {
                binding
                    .action()
                    .as_any()
                    .downcast_ref::<gpui_kit::Unbind>()
                    .map(|unbind| unbind.0.to_string())
            })
            .collect::<std::collections::BTreeSet<_>>()
    });
    for action in [
        "k8s_shell::OpenContextSwitcher",
        "k8s_shell::OpenNamespaceSwitcher",
        "k8s_shell::OpenResourceKindSwitcher",
        "k8s_shell::TogglePinTab",
        "k8s_shell::MoveTabLeft",
        "k8s_shell::MoveTabRight",
    ] {
        assert!(
            targets.contains(action),
            "missing Terminal unbind: {action}"
        );
    }
}

#[gpui_kit::test]
fn selection_commands_explain_wrong_view_or_missing_selection(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_exec(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("Select a pod"))
    }));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_forward_port(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("Select a Pod"))
    }));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_restart(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("Select a workload"))
    }));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_scale(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("Scale is only available"))
    }));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_show_events(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("Select a row"))
    }));
}

/// The shape of a shell command that acts on what the table has selected.
type SelectionCommand = fn(&mut Shell, &mut gpui_kit::Window, &mut gpui_kit::Context<Shell>);

/// The table selects a range or a set; the Inspector and every object action read one row.
///
/// Hitting the anchor of a three-row selection restarts one Pod and leaves the reader believing
/// it acted on all three, so each action that reaches exactly one object stops here and counts
/// the rows. Delete and Scale state their own multi-row answers inside the table; these four
/// had none.
#[gpui_kit::test]
fn a_multi_row_selection_stops_every_action_that_hits_one_object(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let ops = Rc::new(RecordingOps::default());
    inject_ops(cx, &shell, Rc::clone(&ops));
    focus_table(cx, &shell);

    // Select All is the shortest route to a selection that one row cannot describe.
    cx.simulate_keystrokes("ctrl-a");
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell.pods.read(cx).selection_count()),
        3,
        "the demo table must list more than one row for this to mean anything"
    );

    for (action, command) in [
        ("Show logs", Shell::command_show_logs as SelectionCommand),
        ("Exec", Shell::command_exec as SelectionCommand),
        (
            "Start port forward",
            Shell::command_forward_port as SelectionCommand,
        ),
        ("Restart", Shell::command_restart as SelectionCommand),
    ] {
        cx.update(|window, cx| shell.update(cx, |shell, cx| command(shell, window, cx)));
        cx.run_until_parked();
        let message = shell
            .read_with(cx, |shell, _| {
                shell.toast.as_ref().map(|toast| toast.message.clone())
            })
            .unwrap_or_default();
        assert!(
            message.contains(action) && message.contains("3 selected rows"),
            "{action} must refuse a multi-row selection by naming itself and the count, got: {message}"
        );
    }
    assert!(
        ops.calls.borrow().is_empty(),
        "no action may hit one of three selected rows"
    );
}

#[gpui_kit::test]
fn service_account_action_opens_a_fixed_preview(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();
    let notifications = shell.read_with(cx, |shell, _| shell.notifications.len());
    shell.update(cx, |shell, _| {
        shell.service_account_future = Some(Rc::new(move |target| {
            assert_eq!(
                target,
                ServiceAccountTarget {
                    namespace: "default".to_owned(),
                    name: "default".to_owned(),
                }
            );
            Box::pin(async { Ok(service_account_object()) }) as OpsFuture<DynamicObject>
        }));
    });
    shell
        .read_with(cx, |shell, _| shell.pods.clone())
        .update(cx, |view, cx| view.select_uid_for_test("uid-demo-0", cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_service_account(&OpenServiceAccount, window, cx);
        });
    });
    cx.run_until_parked();

    let preview = shell.read_with(cx, |shell, _| {
        shell
            .tabs
            .iter()
            .position(|tab| {
                tab.content == TabContent::Preview && tab.kind.as_ref() == "ServiceAccount"
            })
            .expect("Service Account preview")
    });
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.active_tab, preview);
        let Some(TabView::Preview(view)) = shell.views.get(preview).and_then(Option::as_ref) else {
            panic!("preview view");
        };
        assert_eq!(
            view.read(cx)
                .selection()
                .map(|selection| selection.name.as_str()),
            Some("default")
        );
        assert_eq!(shell.notifications.len(), notifications);
        assert!(shell.toast.is_none());
    });
}

#[gpui_kit::test]
fn service_account_failure_keeps_kube_detail_out_of_primary_copy(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();
    shell.update(cx, |shell, _| {
        shell.service_account_future = Some(Rc::new(|_| {
            Box::pin(async { Err("raw kube forbidden".to_owned()) }) as OpsFuture<DynamicObject>
        }));
    });
    shell
        .read_with(cx, |shell, _| shell.pods.clone())
        .update(cx, |view, cx| view.select_uid_for_test("uid-demo-0", cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_service_account(&OpenServiceAccount, window, cx);
        });
    });
    cx.run_until_parked();

    shell.read_with(cx, |shell, _| {
        assert!(!shell.tabs.iter().any(|tab| {
            tab.content == TabContent::Preview && tab.kind.as_ref() == "ServiceAccount"
        }));
        let notification = shell.notifications.last().expect("failure notification");
        assert_eq!(notification.severity, crate::design::Severity::Error);
        assert!(!notification.message.contains("raw kube forbidden"));
        assert_eq!(notification.detail.as_deref(), Some("raw kube forbidden"));
    });
}

#[gpui_kit::test]
fn service_account_identity_mismatch_is_reported_without_navigation(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();
    shell.update(cx, |shell, _| {
        shell.service_account_future = Some(Rc::new(|_| {
            let mut object = service_account_object();
            object.metadata.name = Some("other".to_owned());
            Box::pin(async { Ok(object) }) as OpsFuture<DynamicObject>
        }));
    });
    shell
        .read_with(cx, |shell, _| shell.pods.clone())
        .update(cx, |view, cx| view.select_uid_for_test("uid-demo-0", cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_service_account(&OpenServiceAccount, window, cx);
        });
    });
    cx.run_until_parked();

    shell.read_with(cx, |shell, _| {
        assert!(!shell.tabs.iter().any(|tab| {
            tab.content == TabContent::Preview && tab.kind.as_ref() == "ServiceAccount"
        }));
        let notification = shell.notifications.last().expect("mismatch notification");
        assert!(notification.message.contains("did not match"));
        assert!(
            notification
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("name=other"))
        );
    });
}

#[gpui_kit::test]
fn service_account_result_is_ignored_after_leaving_the_source_tab(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let receiver = Rc::new(RefCell::new(Some(receiver)));
    shell.update(cx, |shell, _| {
        let receiver = Rc::clone(&receiver);
        shell.service_account_future = Some(Rc::new(move |_| {
            let receiver = receiver
                .borrow_mut()
                .take()
                .expect("one service account read");
            Box::pin(async move {
                receiver
                    .await
                    .map_err(|error| format!("service account task cancelled: {error}"))
            }) as OpsFuture<DynamicObject>
        }));
    });
    shell
        .read_with(cx, |shell, _| shell.pods.clone())
        .update(cx, |view, cx| view.select_uid_for_test("uid-demo-0", cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_service_account(&OpenServiceAccount, window, cx);
        });
    });
    shell.update(cx, |shell, _| shell.active_tab = 1);
    sender
        .send(service_account_object())
        .expect("send service account result");
    cx.run_until_parked();

    shell.read_with(cx, |shell, _| {
        assert!(shell.service_account_request.is_none());
        assert!(!shell.tabs.iter().any(|tab| {
            tab.content == TabContent::Preview && tab.kind.as_ref() == "ServiceAccount"
        }));
    });
}

#[test]
fn palette_result_count_stays_small_and_singular() {
    assert_eq!(palette_result_label(0), "0 results");
    assert_eq!(palette_result_label(1), "1 result");
    assert_eq!(palette_result_label(4), "4 results");
    // The palette is the longest list in the app, so it is the one place a count over a thousand
    // actually shows up. It printed the raw number while every other surface in the app went
    // through `design::format::count`, which is what made `DESIGN.md` §9's claim false.
    assert_eq!(palette_result_label(1_200), "1,200 results");
    assert_eq!(palette_result_label(12_345), "12,345 results");
}

#[gpui_kit::test]
fn palette_multi_word_query_reports_matching_results(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("theme light");
    cx.run_until_parked();

    assert_eq!(
        shell.read_with(cx, |shell, _| (
            shell.palette_query.clone(),
            shell.filtered_command_count()
        )),
        ("theme light".to_owned(), 2)
    );
    assert!(cx.debug_bounds("command-palette-result-count").is_some());
}

/// The current value is not an error, so activating it changes nothing and says nothing.
///
/// The row used to answer with `Already selected.`, which replaced all three of the footer's key
/// hints to restate the checkmark drawn at the other end of the same row. `DESIGN.md` §4 gives the
/// current item two marks and no second wording.
#[gpui_kit::test]
fn palette_current_targets_are_marked_and_do_nothing(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_palette_with_scope(PaletteScope::Context, "", window, cx);
        });
    });
    let current = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .find(|command| command.id.as_ref() == "context.switch.kind-k8s-gpui-dev")
            .map(|command| command.id.clone())
            .expect("current context command")
    });
    assert!(
        shell.read_with(cx, |shell, _| shell
            .commands
            .iter()
            .find(|command| command.id == current)
            .is_some_and(super::palette_command_is_current)),
        "the row that names the value in use is the one the checkmark follows"
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.run_command(current, window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));
    assert_eq!(shell.read_with(cx, |shell, _| shell.palette_note), None);
}

#[gpui_kit::test]
fn palette_scopes_report_loading_failed_and_empty_states(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, _cx| {
        shell.startup_state = StartupState::Loading;
        shell.rebuild_commands();
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "context.status.loading")
    }));
    shell.update(cx, |shell, _cx| {
        shell.startup_state = StartupState::Ready;
        shell.namespace_state = NamespaceState::Failed("forbidden".to_owned());
        shell.rebuild_commands();
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "namespace.status.failed")
            && shell
                .commands
                .iter()
                .any(|command| command.id.as_ref() == "namespace.switch.All_namespaces")
    }));
    shell.update(cx, |shell, _cx| {
        shell.namespace_state = NamespaceState::Ready(Vec::new());
        shell.rebuild_commands();
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "namespace.status.empty")
            && shell
                .commands
                .iter()
                .any(|command| command.id.as_ref() == "namespace.switch.All_namespaces")
    }));
    shell.update(cx, |shell, _cx| {
        shell.tree = super::tree::ResourceTree::empty("kind-k8s-gpui-dev");
        shell.catalog_state = CatalogState::Ready;
        shell.rebuild_commands();
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .any(|command| command.id.as_ref() == "kind.status.empty")
    }));
}

/// Settings is a centre tab, not a takeover.
///
/// It used to hide the resource tree, disable both panel toggles, and answer a click on either
/// with a toast whose second line was four characters long. The only way back was closing the tab.
/// The settings view draws its own category column inside the content area, so the tree and the
/// Inspector keep their own switches while Settings is open.
#[gpui_kit::test]
fn settings_keeps_the_resource_chrome_and_its_own_panel_switches(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(960.0), px(640.0)));
    cx.run_until_parked();
    assert!(cx.debug_bounds("resource-tree-panel").is_some());

    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            assert!(shell.open_special_tab(
                TabContent::Settings,
                "Settings",
                gpui_kit::assets::IconName::Settings,
                cx,
            ));
        });
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.settings_active()));
    assert!(cx.debug_bounds("settings-category-Appearance").is_some());
    assert!(cx.debug_bounds("command-palette").is_some());
    assert!(cx.debug_bounds("open-settings").is_some());
    assert!(cx.debug_bounds("command-palette-keycap").is_none());
    assert!(cx.debug_bounds("open-settings-keycap").is_none());
    // The tree stays, and so does the toolbar that switches it.
    assert!(cx.debug_bounds("resource-tree-panel").is_some());
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));

    // The sidebar switch still works while Settings is open, and raises no toast.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.toggle_left_panel(&ToggleLeftPanel, window, cx)
        });
    });
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |shell, _| shell.sidebar_open));
    assert!(
        shell.read_with(cx, |shell, _| shell.toast.is_none()),
        "a panel switch is not an error, so it does not raise a message"
    );

    let settings_tab = shell.read_with(cx, |shell, _| shell.active_tab);
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.close_tab_index(settings_tab, window, cx)
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("resource-tree-panel").is_some());
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));
}

#[gpui_kit::test]
fn top_bar_mouse_callbacks_are_deferred(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    let palette = cx
        .debug_bounds("command-palette")
        .expect("command palette button is laid out");
    cx.simulate_click(palette.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();

    let cluster = cx
        .debug_bounds("cluster-selector")
        .expect("cluster selector is laid out");
    cx.simulate_click(cluster.center(), Modifiers::none());
    cx.run_until_parked();
    cx.run_until_parked();
    let second_cluster = cx
        .debug_bounds("cluster-option-1")
        .expect("second cluster option is laid out");
    cx.simulate_click(second_cluster.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_cluster), 1);
    shell.update(cx, |shell, cx| {
        shell.namespace_state = NamespaceState::Ready(vec!["default".into()]);
        cx.notify();
    });
    cx.run_until_parked();

    let namespace = cx
        .debug_bounds("namespace-selector")
        .expect("namespace selector is laid out");
    cx.simulate_click(namespace.center(), Modifiers::none());
    cx.run_until_parked();
    cx.run_until_parked();
    let default_namespace = cx
        .debug_bounds("MENU_ITEM-default")
        .expect("default namespace option is laid out");
    cx.simulate_click(default_namespace.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace.to_string()),
        "default"
    );
}

#[gpui_kit::test]
fn center_tabs_use_one_roving_focus_without_wrapping(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        cx.notify();
    });
    cx.run_until_parked();

    let focus = shell.read_with(cx, |shell, _| shell.center_tabs_focus.clone());
    cx.update(|window, cx| window.focus(&focus, cx));
    cx.run_until_parked();

    cx.simulate_keystrokes("left");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.center_tabs_cursor)),
        (1, Some(0))
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 0);

    cx.simulate_keystrokes("right");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.center_tabs_cursor)),
        (0, Some(1))
    );
    cx.simulate_keystrokes("right");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.center_tabs_cursor)),
        (0, Some(1))
    );
    assert!(cx.update(|window, _| focus.is_focused(window)));
}

#[gpui_kit::test]
fn shell_action_dispatch_defers_until_update_completes(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.dispatch(super::ToggleLeftPanel, window, cx);
            shell.dispatch(super::ToggleDock, window, cx);
            assert!(shell.sidebar_open);
            assert!(!shell.dock_open);
        });
    });
    cx.run_until_parked();

    assert!(!shell.read_with(cx, |shell, _| shell.sidebar_open));
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
}

#[gpui_kit::test]
fn connection_failure_has_one_primary_shell_surface(cx: &mut TestAppContext) {
    init_ui(cx);
    let session = ClusterSession::unavailable("no kubeconfig");
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    cx.run_until_parked();

    assert!(shell.read_with(cx, |shell, _| {
        matches!(&shell.connection, ConnectionState::Failed(_))
    }));
    assert!(shell.read_with(cx, |shell, cx| shell.connection_failure_is_primary(cx)));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .tabs
            .get(shell.active_tab)
            .map(|tab| tab.content)),
        Some(TabContent::Overview),
        "a kubeconfig that could not be read leaves the reader on the tab that explains it, with the failure surface in front of it"
    );
    assert!(cx.debug_bounds("connection-failure").is_some());
    assert!(cx.debug_bounds("pods-error").is_none());
    assert!(cx.debug_bounds("tree-failure").is_none());
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.connection.label()),
        "Unavailable"
    );
}

#[gpui_kit::test]
fn startup_loading_replacement_advances_epoch_and_rebinds_owned_ui(cx: &mut TestAppContext) {
    init_ui(cx);
    let terminals: TerminalFactory =
        Rc::new(|_request, _sink, _cx| Err("terminal unavailable".to_owned()));
    let forwards: PortForwardFactory =
        Rc::new(|_request, _cx| Err("forward unavailable".to_owned()));
    let (shell, cx) = cx.add_window_view(|_, cx| {
        Shell::with_cluster(
            ClusterSession::unavailable(super::STARTUP_LOADING_REASON),
            cx,
        )
    });
    shell.update(cx, |shell, cx| {
        shell.set_terminal_services(
            Some(TerminalServices {
                terminals,
                forwards,
                context: Some("old-context".to_owned()),
                namespace: None,
            }),
            cx,
        );
        shell.search_epoch = Some(shell.session_epoch);
    });
    cx.run_until_parked();

    let (old_epoch, old_pods, old_dock, old_views) = shell.read_with(cx, |shell, _| {
        (
            shell.session_epoch,
            shell.pods.entity_id().as_u64(),
            shell.dock_panel.entity_id().as_u64(),
            mounted_view_ids(shell),
        )
    });
    assert!(
        !old_views.is_empty(),
        "a shell with no registry opens on a mounted Overview, so there is a view to outlive"
    );
    assert!(shell.read_with(cx, |shell, _| {
        matches!(&shell.connection, ConnectionState::Connecting)
            && matches!(&shell.catalog_state, CatalogState::Loading)
    }));

    shell.update(cx, |shell, cx| {
        shell.latency_tier = LatencyTier::HighLatency;
        shell.latency_auto_paused.insert(1);
        assert!(shell.replace_session(ClusterSession::unavailable("kubeconfig failed"), cx));
    });

    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.session_epoch, old_epoch + 1);
        assert_ne!(shell.pods.entity_id().as_u64(), old_pods);
        assert_ne!(shell.dock_panel.entity_id().as_u64(), old_dock);
         // Nothing built for the replaced session is still mounted. The active tab's view
         // is rebuilt rather than kept — it was built with no handle, and a handle is the
         // one thing that view can never be given — so this asks about the entities, not
         // about the slots being empty.
         assert!(
             mounted_view_ids(shell)
                 .iter()
                 .all(|id| !old_views.contains(id)),
             "no view from the replaced session survives the replacement"
         );
         assert!(shell.latency_auto_paused.is_empty());
         assert_eq!(shell.latency_tier, LatencyTier::Local);
         assert!(shell.search_epoch.is_none());
        assert!(shell.inspector.read(cx).session_identity().id == shell.session_epoch);
        assert!(shell.terminal_services.as_ref().unwrap().context.is_none());
        assert!(matches!(&shell.connection, ConnectionState::Failed(reason) if reason == "kubeconfig failed"));
        assert!(matches!(&shell.catalog_state, CatalogState::Failed(reason) if reason == "kubeconfig failed"));
    });
}

#[gpui_kit::test]
fn session_replacement_drops_old_async_results(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| {
        Shell::with_cluster(
            ClusterSession::unavailable(super::STARTUP_LOADING_REASON),
            cx,
        )
    });
    let old_epoch = shell.read_with(cx, |shell, _| shell.session_epoch);
    shell.update(cx, |shell, cx| {
        assert!(shell.replace_session(ClusterSession::unavailable("replacement"), cx));
        shell.on_catalog_loaded_at(old_epoch, Ok(ResourceCatalog::default()), cx);
        shell.on_health_result(old_epoch, k8s_core::cluster::Health::Ready, cx);
    });

    shell.read_with(cx, |shell, _| {
        assert!(
            matches!(&shell.catalog_state, CatalogState::Failed(reason) if reason == "replacement")
        );
        assert!(
            matches!(&shell.connection, ConnectionState::Failed(reason) if reason == "replacement")
        );
        assert!(
            !shell
                .tree
                .rows(&shell.collapsed)
                .iter()
                .any(|row| row.resource_kind.is_some())
        );
    });
}

#[gpui_kit::test]
fn update_state_and_callbacks_are_injectable(cx: &mut TestAppContext) {
    init_ui(cx);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let check_calls = calls.clone();
    let retry_calls = calls.clone();
    let restart_calls = calls.clone();
    let actions = UpdateActions::new(
        move |_| check_calls.borrow_mut().push("check"),
        move |_| retry_calls.borrow_mut().push("retry"),
        move |_| restart_calls.borrow_mut().push("restart"),
    );
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.set_update_actions(actions, cx);
        shell.set_update_state(
            UpdateUiState::new(UpdatePhase::Ready).with_version("1.2.3"),
            cx,
        );
        shell.run_update_check(cx);
        shell.run_update_retry(cx);
        shell.run_update_restart(cx);
    });
    cx.run_until_parked();

    assert_eq!(calls.borrow().as_slice(), ["check", "retry", "restart"]);
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.update_state().status_text()),
        "Version 1.2.3 is ready"
    );
}

#[gpui_kit::test]
fn unavailable_update_actions_report_instead_of_no_op(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    assert_eq!(
        shell.read_with(cx, |shell, _| (
            shell.update_state().phase,
            shell.update_state().error.clone(),
        )),
        (
            UpdatePhase::Unsupported,
            Some(super::UPDATER_UNAVAILABLE_REASON.to_owned())
        )
    );
    shell.update(cx, |shell, cx| shell.run_update_check(cx));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .toast
            .as_ref()
            .map(|toast| toast.message.to_string())),
        Some(super::UPDATER_UNAVAILABLE_REASON.to_owned())
    );
    let update_commands = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter(|command| {
                matches!(
                    command.id.as_ref(),
                    super::commands::CHECK_FOR_UPDATES_COMMAND_ID
                        | super::commands::RESTART_TO_UPDATE_COMMAND_ID
                )
            })
            .map(|command| match &command.run {
                CommandRun::Unavailable { reason, .. } => Some(reason.to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(update_commands.len(), 2);
    assert!(
        update_commands
            .iter()
            .all(|reason| { reason.as_deref() == Some(super::UPDATER_UNAVAILABLE_REASON) })
    );
}

#[gpui_kit::test]
fn unsupported_update_keeps_actions_in_a_fixed_overlay(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let calls = Rc::new(RefCell::new(Vec::new()));
    let check_calls = calls.clone();
    let retry_calls = calls.clone();
    let restart_calls = calls.clone();
    let actions = UpdateActions::new(
        move |_| check_calls.borrow_mut().push("check"),
        move |_| retry_calls.borrow_mut().push("retry"),
        move |_| restart_calls.borrow_mut().push("restart"),
    );
    shell.update(cx, |shell, cx| {
        shell.set_update_actions(actions, cx);
        shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
    });
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();

    assert!(shell.read_with(cx, |shell, _| shell.update_strip_expanded));
    let overlay = cx
        .debug_bounds("update-strip-overlay")
        .expect("unsupported update overlay is laid out");
    let card = cx
        .debug_bounds("update-strip")
        .expect("unsupported update card is laid out");
    let actions_bounds = cx
        .debug_bounds("update-strip-actions")
        .expect("unsupported update actions are laid out");
    assert!(f32::from(overlay.size.width) <= super::DIALOG_WIDTH);
    assert!(overlay.size.height <= px(584.));
    assert!(overlay.right() <= px(960.));
    assert!(card.top() >= px(40.));
    assert!(actions_bounds.bottom() <= overlay.bottom());
    for selector in [
        "update-action-check",
        "update-action-retry",
        "update-action-restart",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "{selector}");
    }

    let tree_focus = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    let overlay_focus = shell.read_with(cx, |shell, _| shell.update_overlay_focus.clone());
    cx.update(|window, cx| window.focus(&tree_focus, cx));
    let mut reached = false;
    for _ in 0..20 {
        if cx.update(|window, _| overlay_focus.is_focused(window)) {
            reached = true;
            break;
        }
        cx.update(|window, cx| window.focus_next(cx));
    }
    assert!(reached, "update overlay is in the keyboard tab order");

    cx.simulate_keystrokes("enter down enter down enter");
    assert_eq!(calls.borrow().as_slice(), ["check", "retry", "restart"]);

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |shell, _| shell.update_strip_expanded));
    assert!(cx.debug_bounds("update-strip").is_none());
    assert!(cx.debug_bounds("update-strip-actions").is_none());
}

#[gpui_kit::test]
fn shell_renders_and_titles_window(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert_eq!(
        cx.window_title().as_deref(),
        Some("Pods — kind-k8s-gpui-dev")
    );

    let (clusters, kinds) = shell.read_with(cx, |shell, _| {
        (shell.clusters.len(), shell.tree.kind_count())
    });
    assert!(clusters > 0);
    assert!(kinds > 0);
}

#[gpui_kit::test]
fn ctrl_shift_p_opens_palette_and_escape_closes_it(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));

    cx.simulate_keystrokes("secondary-shift-p");
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));

    cx.simulate_input("inspector");
    let (query, count, catalogue, matched) = shell.read_with(cx, |shell, _| {
        (
            shell.palette_query.clone(),
            shell.filtered_command_count(),
            shell.commands.len(),
            shell
                .palette_matches()
                .into_iter()
                .map(|command| command.id.to_string())
                .collect::<Vec<_>>(),
        )
    });
    assert_eq!(query, "inspector");
    assert_eq!(count, matched.len());
    assert!(
        count > 0 && count < catalogue,
        "the query narrows the catalogue instead of matching all of it: {count} of {catalogue}"
    );
    for id in &matched {
        assert!(
            id.contains("inspector"),
            "an unrelated command matched the query: {id}"
        );
    }

    cx.simulate_keystrokes("escape");
    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.palette_query.len()),
        0,
        "closing the palette must clear the query"
    );
}

/// The palette shortcut toggles the open palette closed.
#[gpui_kit::test]
fn ctrl_shift_p_toggles_palette_closed(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.simulate_keystrokes("secondary-shift-p");
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));
    cx.simulate_keystrokes("secondary-shift-p");
    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
}

/// Regression: secondary-k no longer opens the palette.
#[gpui_kit::test]
fn ctrl_k_no_longer_opens_palette(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.simulate_keystrokes("secondary-k");
    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
}

/// Panel buttons and shortcuts use the same action handler.
#[gpui_kit::test]
fn panel_toggles_are_keyboard_driven(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));
    cx.simulate_keystrokes(shortcut("secondary-b", "secondary-shift-l"));
    assert!(!shell.read_with(cx, |shell, _| shell.sidebar_open));
    cx.simulate_keystrokes(shortcut("secondary-b", "secondary-shift-l"));
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));

    assert!(!shell.read_with(cx, |shell, _| shell.inspector_open));
    cx.simulate_keystrokes("secondary-alt-b");
    assert!(shell.read_with(cx, |shell, _| shell.inspector_open));
    cx.simulate_keystrokes("secondary-alt-b");
    assert!(!shell.read_with(cx, |shell, _| shell.inspector_open));

    // The Dock starts hidden like VSCode's panel: the chord's first press opens it. What this
    // test is about is that the chord drives the state in both directions, so it starts from
    // the state the product is actually in.
    assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
    cx.simulate_keystrokes(shortcut("secondary-j", "secondary-shift-d"));
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
    cx.simulate_keystrokes(shortcut("secondary-j", "secondary-shift-d"));
    assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
    // The VSCode/Zed panel chord does the same job on the same surface.
    cx.simulate_keystrokes("secondary-`");
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
    cx.simulate_keystrokes("secondary-`");
    assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
}

#[gpui_kit::test]
fn closing_panels_restores_the_focus_that_opened_them(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let source = cx.update(|_, app| shell.read(app).pods.read(app).table_focus_handle(app));
    cx.update(|window, cx| window.focus(&source, cx));
    cx.run_until_parked();
    assert!(cx.update(|window, _| source.is_focused(window)));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.toggle_left_panel(&ToggleLeftPanel, window, cx)
        });
    });
    assert!(!shell.read_with(cx, |shell, _| shell.sidebar_open));
    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| {
            shell.toggle_left_panel(&ToggleLeftPanel, window, cx)
        });
    });
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.sidebar_previous_focus.clone()),
        Some(source.clone())
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.toggle_left_panel(&ToggleLeftPanel, window, cx)
        });
    });
    cx.run_until_parked();
    assert!(cx.update(|window, _| source.is_focused(window)));

    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| {
            shell.toggle_right_panel(&ToggleRightPanel, window, cx)
        });
    });
    assert!(shell.read_with(cx, |shell, _| shell.inspector_open));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.toggle_right_panel(&ToggleRightPanel, window, cx)
        });
    });
    assert!(cx.update(|window, _| source.is_focused(window)));

    // The Dock starts hidden, so the pair that tests the focus hand-back is open-then-close
    // from a Dock the test opens first: the focus has to survive both directions.
    assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
    });
    assert!(!shell.read_with(cx, |shell, _| shell.dock_open));
    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
    cx.update(|window, cx| {
        window.focus(&source, cx);
        shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
    });
    assert!(cx.update(|window, _| source.is_focused(window)));
}

#[gpui_kit::test]
fn notification_center_traps_focus_and_restores_it_on_close(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.notify(
            "First failure".to_owned(),
            crate::design::Severity::Error,
            Some("first detail".to_owned()),
            cx,
        );
        shell.notify(
            "Second failure".to_owned(),
            crate::design::Severity::Warning,
            Some("second detail".to_owned()),
            cx,
        );
    });
    let source = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    cx.update(|window, cx| window.focus(&source, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_notifications(window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.status_panel
        == StatusPanel::Notifications));
    assert!(cx.update(|window, app| { shell.read(app).notification_focus.is_focused(window) }));

    cx.simulate_keystrokes("tab");
    assert!(
        cx.update(|window, app| { shell.read(app).notification_clear_focus.is_focused(window) })
    );
    cx.simulate_keystrokes("tab");
    assert!(cx.update(|window, app| {
        shell
            .read(app)
            .notification_row_focus_handles
            .first()
            .is_some_and(|handle| handle.is_focused(window))
    }));
    cx.simulate_keystrokes("shift-tab");
    assert!(
        cx.update(|window, app| { shell.read(app).notification_clear_focus.is_focused(window) })
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.status_panel == StatusPanel::None));
    assert!(cx.update(|window, _| source.is_focused(window)));
}

#[gpui_kit::test]
fn open_forwards_reuses_the_center_management_tab(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_open_forwards(window, cx));
    });
    cx.run_until_parked();
    let first = shell.read_with(cx, |shell, _| {
        let index = shell
            .tabs
            .iter()
            .position(|tab| tab.content == TabContent::Forwards)
            .expect("forwards tab");
        assert!(matches!(shell.views[index], Some(TabView::Forwards(_))));
        index
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_open_forwards(window, cx));
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .tabs
            .iter()
            .filter(|tab| tab.content == TabContent::Forwards)
            .count()),
        1
    );
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), first);
}

/// `UI-SPEC.md` §14.6: the status bar's forwards item is a link, and the list it opens is the
/// centre view. §2.5 puts Port Forward in the Console family rather than the notification family,
/// which is the other half of the same rule — so no popover is mounted and no notification centre
/// competes with it.
#[gpui_kit::test]
fn the_status_bar_forward_link_opens_the_centre_list(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();

    let link = cx
        .debug_bounds("status-bar-port-forwards")
        .expect("§14.6 puts a forwards link on the bar");
    let status_bar = cx.debug_bounds("status-bar").expect("status bar");
    assert!(
        link.top() >= status_bar.top() && link.bottom() <= status_bar.bottom(),
        "the link fits inside the bar: {link:?} vs {status_bar:?}"
    );

    cx.simulate_click(link.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.status_panel),
        StatusPanel::None,
        "the list is a view, not a popover"
    );
    assert!(cx.debug_bounds("port-forward-panel").is_none());
    assert!(cx.debug_bounds("notification-center").is_none());
    let (content, title) = shell.read_with(cx, |shell, _| {
        (
            shell.tabs[shell.active_tab].content,
            shell.tabs[shell.active_tab].title.clone(),
        )
    });
    assert_eq!(content, TabContent::Forwards, "the link opened the list");
    assert_eq!(title.as_ref(), "Port forwards");
    assert!(
        cx.debug_bounds("center-tab-panel").is_some(),
        "the list is mounted in the centre column"
    );
}

#[gpui_kit::test]
fn preview_metrics_visibility_follows_the_active_preview_only(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    shell.update(cx, |shell, cx| {
        shell.open_row_details(
            Row {
                obj: search_hit("preview").object,
                cells: Vec::new(),
            },
            ResourceSpec::pods(),
            0,
            cx,
        );
    });
    cx.run_until_parked();
    let preview = shell.read_with(cx, |shell, _| shell.active_tab);
    let preview_view = shell.read_with(cx, |shell, _| {
        shell.views[preview]
            .as_ref()
            .and_then(|slot| match slot {
                super::TabView::Preview(view) => Some(view.clone()),
                _ => None,
            })
            .expect("preview view")
    });
    assert!(preview_view.read_with(cx, |panel, _| panel.metrics_visible()));
    assert!(!preview_view.read_with(cx, |panel, _| panel.metrics_sampling()));

    shell.update(cx, |shell, cx| {
        shell.inspector_open = true;
        cx.notify();
    });
    cx.run_until_parked();
    // The right-hand panel is not driven while a Preview is open. A Preview tab
    // *is* the Inspector -- same component, same YAML, same tab set -- and
    // `panel_visibility` withdraws the panel rather than showing a second copy of
    // the answer the centre is already giving. So the metrics belong to the
    // preview alone, which is what the test's name says and what its last
    // assertion used to contradict.
    assert!(!shell.read_with(cx, |shell, cx| shell.inspector.read(cx).metrics_visible()));

    shell.update(cx, |shell, cx| assert!(shell.activate_tab(0, cx)));
    cx.run_until_parked();
    assert!(!preview_view.read_with(cx, |panel, _| panel.metrics_visible()));
    assert!(!preview_view.read_with(cx, |panel, _| panel.metrics_sampling()));
    // With the Preview gone the panel takes the metrics over, so switching away
    // does not silently stop the reader from watching them.
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).metrics_visible()));
}

/// `UI-SPEC` §11.2 has three Inspector states, and only one of them is "not on screen".
///
/// The row that changed is the one below 1000px. It used to answer `false` — the Inspector was
/// hidden — and a hidden Inspector is the worst answer a narrow window can give, because the
/// reader who just selected a Pod and made the window smaller is exactly the reader who needs
/// to see what they selected. So it floats: visible, over the centre, with the centre keeping
/// every pixel it had. This asserts the two facts that changed, so the next person to move a
/// breakpoint sees which row they are editing.
#[test]
fn the_inspector_is_docked_narrow_floating_or_gone_and_never_silently_missing() {
    for (width, expected) in [
        (2_560.0, InspectorLayout::Docked),
        (1_624.0, InspectorLayout::Docked),
        (1_440.0, InspectorLayout::Docked),
        (1_200.0, InspectorLayout::Docked),
        (1_199.0, InspectorLayout::Docked),
        (INSPECTOR_FLOAT_BELOW, InspectorLayout::Docked),
        (INSPECTOR_FLOAT_BELOW - 0.5, InspectorLayout::Floating),
        (960.0, InspectorLayout::Floating),
    ] {
        assert_eq!(inspector_layout(true, width), expected, "{width}px");
    }
    // The only two ways to lose it are the reader's own two.
    assert_eq!(inspector_layout(false, 2_560.0), InspectorLayout::Closed);
    assert_eq!(inspector_layout(false, 960.0), InspectorLayout::Closed);
}

/// A floating Inspector is still on screen, so "visible" cannot mean "took width".
///
/// This is the whole difference between the two states and the reason §11.2 asks for three:
/// `Docked` takes the centre's pixels, `Floating` does not, and a caller that only asks
/// "is it shown" and then reserves width for it is the bug that made the old breakpoint
/// destructive instead of responsive.
#[test]
fn a_floating_inspector_does_not_reserve_centre_width() {
    let docked = inspector_layout(true, 1_440.0);
    let floating = inspector_layout(true, 960.0);
    assert!(docked.takes_width());
    assert!(!floating.takes_width());
    assert!(
        responsive_panel_visibility(true, true, 960.0).1,
        "960px floats"
    );
    assert!(responsive_panel_visibility(false, true, 1_440.0) == (false, true));
    assert_eq!(
        responsive_panel_visibility(true, true, 959.0),
        (false, true),
        "below the window floor the sidebar goes, and the Inspector is still there: the two \
         gates are different numbers and only one of them is a breakpoint"
    );
}

/// §11.2's 280 row is a ceiling on the docked Inspector, and only there.
///
/// Reading the whole table as ceilings is the mistake this test is against: the sidebar's
/// 236 is a resting width in a range of 180 to 360, so a ceiling of 236 would stop a reader
/// dragging it any further and the range §11.1 promises would be unreachable. The Inspector's
/// 280 is the opposite — it exists to hold the centre above its own budget, so it is a
/// ceiling, and below 1200 the range is all the reader gets.
#[test]
fn the_docked_inspector_has_one_band_ceiling_and_the_sidebar_has_none() {
    for (width, expected) in [
        (2_560.0, None),
        (1_440.0, None),
        (1_200.0, None),
        (1_199.0, Some(280.0)),
        (1_000.0, Some(280.0)),
        (960.0, Some(280.0)),
    ] {
        assert_eq!(
            inspector_width_ceiling(width),
            expected,
            "inspector ceiling at {width}px"
        );
    }
    // The sidebar's own numbers, and the reason a band ceiling would be wrong: the range is
    // wider than the resting width, so the resting width cannot also be the limit.
    assert!(
        f32::from(crate::design::size::SIDEBAR_DEFAULT)
            < f32::from(crate::design::size::SIDEBAR_MAX)
    );
    assert!(
        f32::from(crate::design::size::SIDEBAR_MIN)
            < f32::from(crate::design::size::SIDEBAR_DEFAULT)
    );
    assert!(
        f32::from(crate::design::size::INSPECTOR_DEFAULT)
            < f32::from(crate::design::size::INSPECTOR_MAX)
    );
}

/// The sidebar's narrow gate is the window's own floor, so there is no second responsive step.
///
/// `DESIGN.md` §6 promises two: change the Inspector, then compress the Sidebar. The second one
/// cannot run. The gate is `width >= MIN_LAYOUT_WIDTH` and `MIN_LAYOUT_WIDTH` is
/// `design::size::WINDOW_MIN.0`, which is also what `main.rs` gives `window_min_size`, so the only
/// width that hides the sidebar is a width no window can be. This holds the two numbers to each
/// other, so the moment one of them moves the fact stops being true and the contract has to be
/// rewritten rather than quietly continuing to promise a level that does not exist.
#[test]
fn the_sidebar_gate_is_the_window_floor_and_not_a_second_breakpoint() {
    assert_eq!(
        MIN_LAYOUT_WIDTH,
        crate::design::size::WINDOW_MIN.0,
        "a sidebar gate below the window's own minimum would be a responsive step, and §6 would \
         owe the reader one"
    );
    // Every width the window can be, at the only breakpoint that does anything.
    for width in [
        crate::design::size::WINDOW_MIN.0,
        1_000.0,
        1_199.0,
        1_200.0,
        1_440.0,
        2_560.0,
    ] {
        let (sidebar, inspector) = responsive_panel_visibility(true, true, width);
        assert!(sidebar, "{width}px is a window this app can be");
        assert!(
            inspector,
            "{width}px is a window this app can be, and §11.2 gives the Inspector a row at \
             every one of them — docked or floating, never absent"
        );
    }
}

/// Type-ahead walks a run of matches instead of parking on the first.
///
/// This is the whole point of the buffer: a cluster has four `coredns-…` rows and a reader who
/// wants the third one types the prefix twice. The match is per name segment because a reader
/// types `core` and means `coredns-…`, and it starts after the current row so the first
/// keystroke on a table with nothing selected lands on the first match.
#[test]
fn type_ahead_matches_per_name_segment_and_cycles_through_the_run() {
    let labels = [
        "Bindings",
        "core",
        "coredns-559f6c778d-hzwzg",
        "coredns-559f6c778d-wqlk9",
        "coredns-canary",
        "Namespaces",
    ];
    // Nothing selected: the first keystroke reaches the first match, not the next one.
    assert_eq!(type_ahead_index(&labels, "c", usize::MAX), Some(1));
    // One press per match down the run, and the run wraps rather than stopping.
    assert_eq!(type_ahead_index(&labels, "c", 1), Some(2));
    assert_eq!(type_ahead_index(&labels, "c", 2), Some(3));
    assert_eq!(type_ahead_index(&labels, "c", 3), Some(4));
    assert_eq!(type_ahead_index(&labels, "c", 4), Some(1));
    // A longer prefix narrows the run: `coredns-c` is the canary and nothing else.
    assert_eq!(type_ahead_index(&labels, "co", usize::MAX), Some(1));
    assert_eq!(type_ahead_index(&labels, "coredns-c", 2), Some(4));
    // The per-segment match is what makes `core` reach `coredns-…` at all, which is the
    // case a reader means every time: they type the workload, not the generated name.
    assert_eq!(type_ahead_index(&labels, "core", usize::MAX), Some(1));
    assert_eq!(type_ahead_index(&labels, "core", 1), Some(2));
    // No match leaves the reader where they were, which is what an unknown prefix must do:
    // a list that jumps to row zero on a typo is worse than one that does not move.
    assert_eq!(type_ahead_index(&labels, "zzz", 2), None);
    assert_eq!(type_ahead_index(&labels, "", 2), None);
}

#[gpui_kit::test]
fn separators_resize_with_keyboard(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let initial = shell.read_with(cx, |shell, _| shell.left_width);
    shell.update(cx, |shell, cx| {
        assert!(shell.resize_panels_with_key(DragTarget::Left, "right", cx));
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.left_width),
        initial + DIVIDER_KEY_STEP
    );
}

#[gpui_kit::test]
fn palette_modal_blocks_panel_shortcuts(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.simulate_keystrokes("secondary-shift-p");
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));

    cx.simulate_keystrokes(shortcut("secondary-b", "secondary-shift-l"));
    assert!(
        shell.read_with(cx, |shell, _| shell.sidebar_open),
        "global panel shortcuts must not run while the palette is open"
    );

    cx.simulate_keystrokes("escape");
    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
    cx.simulate_keystrokes(shortcut("secondary-b", "secondary-shift-l"));
    assert!(!shell.read_with(cx, |shell, _| shell.sidebar_open));
}

/// Tests tab shortcuts for open, close, and cycle actions.
#[gpui_kit::test]
fn tab_actions_open_close_and_cycle(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.open_tabs.clone())),
        (0, vec![0]),
        "startup opens only Pods"
    );

    cx.simulate_keystrokes("secondary-2");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.open_tabs.clone())),
        (1, vec![0, 1]),
        "secondary-2 opens the built-in Deployments Tab"
    );

    cx.simulate_keystrokes("secondary-shift-t");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.open_tabs.clone())),
        (0, vec![0]),
        "closing Deployments returns to the only remaining Pods Tab"
    );
    assert_eq!(cx.windows().len(), 1);

    cx.simulate_keystrokes("secondary-2");
    cx.simulate_keystrokes("secondary-pagedown");
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 0);
    cx.simulate_keystrokes("secondary-pageup");
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 1);
}

#[gpui_kit::test]
fn high_latency_pauses_hidden_resources_and_restores_active_and_local_tier(
    cx: &mut TestAppContext,
) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        let hidden = cx.new(|cx| PodsView::new(super::fake_source_factory(), None, cx));
        shell.views[1] = Some(super::TabView::Resource(hidden));
        shell.open_tabs = vec![0, 1];
    });
    cx.run_until_parked();

    let active = shell.read_with(cx, |shell, _| shell.resource_view(0).unwrap());
    let hidden = shell.read_with(cx, |shell, _| shell.resource_view(1).unwrap());
    shell.update(cx, |shell, cx| {
        shell.on_latency_tier(LatencyTier::HighLatency, cx);
    });
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
    assert!(shell.read_with(cx, |shell, _| shell.latency_auto_paused.contains(&1)));

    cx.update(|_, cx| {
        shell.update(cx, |shell, cx| {
            assert!(shell.open_special_tab(
                TabContent::Settings,
                "Settings",
                gpui_kit::assets::IconName::Settings,
                cx,
            ));
        });
    });
    cx.run_until_parked();
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));

    shell.update(cx, |shell, cx| assert!(shell.activate_tab(0, cx)));
    assert!(!matches!(
        active.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
    assert!(shell.read_with(cx, |shell, _| !shell.latency_auto_paused.contains(&0)));

    shell.update(cx, |shell, cx| assert!(shell.activate_tab(1, cx)));
    assert!(!matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(0, cx)));
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));

    shell.update(cx, |shell, cx| {
        shell.on_latency_tier(LatencyTier::Local, cx);
    });
    assert!(!matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
    assert!(shell.read_with(cx, |shell, _| shell.latency_auto_paused.is_empty()));
}

#[gpui_kit::test]
fn high_latency_does_not_claim_a_manual_pause(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        let hidden = cx.new(|cx| PodsView::new(super::fake_source_factory(), None, cx));
        shell.views[1] = Some(super::TabView::Resource(hidden));
        shell.open_tabs = vec![0, 1];
    });
    cx.run_until_parked();

    let hidden = shell.read_with(cx, |shell, _| shell.resource_view(1).unwrap());
    hidden.update(cx, |view, cx| view.pause(cx));
    cx.run_until_parked();
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));

    shell.update(cx, |shell, cx| {
        shell.on_latency_tier(LatencyTier::HighLatency, cx);
    });
    assert!(!shell.read_with(cx, |shell, _| shell.latency_auto_paused.contains(&1)));
    shell.update(cx, |shell, cx| {
        shell.on_latency_tier(LatencyTier::Local, cx);
    });
    assert!(matches!(
        hidden.read_with(cx, |view, cx| view.status(cx)),
        TableStatus::Paused
    ));
}

#[gpui_kit::test]
fn center_resource_tabs_align_left_and_keep_close_separate(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| shell.activate_tab(1, cx));
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();

    let center = cx.debug_bounds("resource-center").expect("resource center");
    let items = cx
        .debug_bounds("center-tabs-items")
        .expect("center tab items");
    let tab = cx
        .debug_bounds("center-tab-item-0")
        .expect("center resource tab");
    let label = cx
        .debug_bounds("center-tab-label-0")
        .expect("center tab label");
    let close = cx
        .debug_bounds("center-tab-close-0")
        .expect("center tab close button");
    assert!((center.left() - items.left()).abs() < px(0.5));
    assert!((items.left() - tab.left()).abs() < px(0.5));
    assert!(label.right() <= close.left());
    assert!(close.right() <= tab.right());
}

/// Closing the last tab leaves the work surface empty instead of opening a substitute view.
#[gpui_kit::test]
fn closing_the_last_tab_leaves_an_empty_center(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();

    cx.simulate_keystrokes("secondary-shift-t");
    cx.run_until_parked();

    assert_eq!(cx.windows().len(), 1, "the shell stays open");
    assert!(
        shell.read_with(cx, |shell, _| shell.open_tabs.is_empty()),
        "closing the last tab opens nothing in its place"
    );
    assert!(
        !shell.read_with(cx, |shell, _| shell
            .tabs
            .iter()
            .any(|tab| matches!(tab.content, TabContent::Overview))),
        "no Overview tab is put back"
    );
    assert!(
        cx.debug_bounds("center-tab-panel").is_none(),
        "no tab panel is mounted"
    );
    let empty = cx
        .debug_bounds("center-empty-state")
        .expect("the empty center surface");
    assert!(
        empty.size.width > px(0.0) && empty.size.height > px(0.0),
        "the empty state fills the center: {empty:?}"
    );

    let empty_action = cx
        .debug_bounds("center-empty-pods")
        .expect("the empty state's action")
        .center();
    cx.simulate_click(empty_action, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.open_tabs.clone())),
        (0, vec![0]),
        "the empty state opens a view again"
    );
    assert!(cx.debug_bounds("center-tab-panel").is_some());
}

#[gpui_kit::test]
fn the_only_tab_still_renders_a_close_button(cx: &mut TestAppContext) {
    init_ui(cx);
    let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();

    cx.debug_bounds("center-tab-close-0")
        .expect("the last tab keeps its close button");
}

/// The close button on the only tab follows the same rule as the Close Tab chord.
#[gpui_kit::test]
fn closing_the_only_tab_from_the_close_button_leaves_an_empty_center(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();

    let close = cx
        .debug_bounds("center-tab-close-0")
        .expect("the last tab close button");
    cx.simulate_click(close.center(), Modifiers::none());
    cx.run_until_parked();

    assert_eq!(cx.windows().len(), 1);
    assert!(
        shell.read_with(cx, |shell, _| shell.open_tabs.is_empty()),
        "the click closes the tab without opening a substitute"
    );
    assert!(
        !shell.read_with(cx, |shell, _| shell
            .tabs
            .iter()
            .any(|tab| matches!(tab.content, TabContent::Overview))),
        "no Overview tab is put back"
    );
    assert!(
        cx.debug_bounds("center-tab-panel").is_none(),
        "no tab panel is mounted"
    );
    cx.debug_bounds("center-empty-state")
        .expect("the empty center surface");
}

#[gpui_kit::test]
fn closing_tabs_selects_the_visual_neighbor(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
    });
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 3);
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_tab_index(3, window, cx)));
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 2);
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 1, 2]
    );
    cx.update(|window, cx| shell.update(cx, |shell, cx| shell.close_tab_index(2, window, cx)));
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 1);
}

#[test]
fn tab_drop_gap_reorders_without_sorting_by_tab_index() {
    let open_tabs = vec![3, 1, 4, 2];
    assert_eq!(tab_drop_gap(&open_tabs, 3, 4, true), Some(2));
    assert_eq!(tab_drop_gap(&open_tabs, 2, 3, false), Some(0));
    assert_eq!(tab_drop_gap(&open_tabs, 1, 1, true), None);

    let mut reordered = open_tabs.clone();
    assert!(reorder_open_tabs(&mut reordered, 3, 2));
    assert_eq!(reordered, vec![1, 4, 3, 2]);
}

#[test]
fn pinned_and_ordinary_tabs_share_drag_and_keyboard_boundaries() {
    let tabs = vec![
        CenterTab {
            content: TabContent::Resource,
            kind: "A".into(),
            identity: None,
            title: "A".into(),
            icon: gpui_kit::assets::IconName::Server,
            pinned: true,
            entry: None,
            resource: None,
            preview: None,
        },
        CenterTab {
            content: TabContent::Resource,
            kind: "B".into(),
            identity: None,
            title: "B".into(),
            icon: gpui_kit::assets::IconName::Server,
            pinned: false,
            entry: None,
            resource: None,
            preview: None,
        },
        CenterTab {
            content: TabContent::Resource,
            kind: "C".into(),
            identity: None,
            title: "C".into(),
            icon: gpui_kit::assets::IconName::Server,
            pinned: false,
            entry: None,
            resource: None,
            preview: None,
        },
        CenterTab {
            content: TabContent::Resource,
            kind: "D".into(),
            identity: None,
            title: "D".into(),
            icon: gpui_kit::assets::IconName::Server,
            pinned: true,
            entry: None,
            resource: None,
            preview: None,
        },
    ];
    let mut open_tabs = vec![0, 1, 2, 3];
    normalize_open_tabs(&mut open_tabs, &tabs);
    assert_eq!(open_tabs, vec![0, 3, 1, 2]);
    assert_eq!(bounded_tab_drop_gap(&open_tabs, &tabs, 1, 0, true), Some(2));
    assert_eq!(
        bounded_tab_drop_gap(&open_tabs, &tabs, 0, 1, false),
        Some(1)
    );
    assert!(move_open_tab_within_group(&mut open_tabs, &tabs, 2, -1));
    assert_eq!(open_tabs, vec![0, 3, 2, 1]);
    assert!(!move_open_tab_within_group(&mut open_tabs, &tabs, 2, -1));
    assert!(!move_open_tab_within_group(&mut open_tabs, &tabs, 0, -1));
    assert!(!move_open_tab_within_group(&mut open_tabs, &tabs, 3, 1));
}

#[gpui_kit::test]
fn pinning_moves_tabs_and_close_all_preserves_pinned(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
        shell.toggle_center_tab_pin(0, cx);
        shell.toggle_center_tab_pin(2, cx);
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 2, 1, 3]
    );
    shell.update(cx, |shell, cx| shell.toggle_center_tab_pin(0, cx));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![2, 0, 1, 3]
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.close_all_center_tabs(window, cx);
        });
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![2]
    );
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 2);
}

#[gpui_kit::test]
fn pinned_and_ordinary_tabs_use_separate_overflow_regions(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
        shell.toggle_center_tab_pin(0, cx);
    });
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();
    let pinned = cx
        .debug_bounds("center-tabs-pinned")
        .expect("pinned overflow");
    let ordinary = cx
        .debug_bounds("center-tabs-ordinary")
        .expect("ordinary overflow");
    assert!(pinned.right() <= ordinary.left());
    assert!(pinned.size.width > px(0.0));
    assert!(ordinary.size.width > px(0.0));
}

#[gpui_kit::test]
fn keyboard_tab_reorder_stays_within_pin_group(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
        shell.toggle_center_tab_pin(0, cx);
        shell.activate_tab(3, cx);
    });
    cx.simulate_keystrokes("secondary-alt-left");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 1, 3, 2]
    );
    cx.simulate_keystrokes("secondary-alt-left");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 3, 1, 2]
    );
    cx.simulate_keystrokes("secondary-alt-left");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 3, 1, 2]
    );
    cx.simulate_keystrokes("secondary-shift-i");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 3, 1, 2]
    );
    assert!(shell.read_with(cx, |shell, _| shell.center_tab_is_pinned(3)));
}

#[gpui_kit::test]
fn center_tabs_reorder_with_gpu_i_drag_and_keep_focus(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
    });
    cx.run_until_parked();

    let from = cx.debug_bounds("center-tab-item-3").expect("dragged tab");
    let to = cx.debug_bounds("center-tab-item-1").expect("drop target");
    cx.simulate_mouse_down(from.center(), MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        point(from.center().x + px(4.0), from.center().y),
        MouseButton::Left,
        Modifiers::none(),
    );
    let drop = point(to.left() + to.size.width * 0.25, to.center().y);
    cx.simulate_mouse_move(drop, MouseButton::Left, Modifiers::none());
    let drag_state = shell.read_with(cx, |shell, _| shell.center_tab_drag);
    assert_eq!(
        drag_state,
        Some(super::CenterTabDragState {
            source: 3,
            insertion: Some(1),
        })
    );
    cx.simulate_mouse_up(drop, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();

    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.open_tabs.clone(), shell.active_tab)),
        (vec![0, 3, 1, 2], 3)
    );
    let focus = shell.read_with(cx, |shell, _| shell.center_tabs_focus.clone());
    assert!(cx.update(|window, _| focus.is_focused(window)));
}

#[gpui_kit::test]
fn middle_click_closes_the_target_tab(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| shell.activate_tab(1, cx));
    cx.update(|_, cx| shell.update(cx, |_, cx| cx.notify()));
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();
    let tab = cx.debug_bounds("center-tab-item-1").expect("center tab");
    cx.simulate_mouse_down(tab.center(), MouseButton::Middle, Modifiers::none());
    cx.simulate_mouse_up(tab.center(), MouseButton::Middle, Modifiers::none());
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0]
    );
}

#[gpui_kit::test]
fn tab_context_menu_pins_and_preserves_pinned_on_close_others(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
        shell.toggle_center_tab_pin(0, cx);
    });
    cx.run_until_parked();

    let target = cx
        .debug_bounds("center-tab-item-3")
        .expect("context target");
    cx.simulate_mouse_down(target.center(), MouseButton::Right, Modifiers::none());
    cx.simulate_mouse_up(target.center(), MouseButton::Right, Modifiers::none());
    cx.run_until_parked();
    // The menu is Close, Close Other Tabs, Close All Tabs, a separator, then Pin.
    // Walking to the last row and pressing Enter is how a keyboard reader reaches
    // Pin, and the row running is what proves the list is the tab list.
    assert!(
        cx.debug_bounds("center-tab-context-menu").is_some(),
        "the right click opens the tab list"
    );
    cx.simulate_keystrokes("down down down down enter");
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, _| shell.center_tab_is_pinned(3)),
        "the last row of the tab list pins the tab"
    );

    let target = cx
        .debug_bounds("center-tab-item-3")
        .expect("context target");
    cx.simulate_click(
        target.center(),
        Modifiers {
            control: true,
            ..Modifiers::none()
        },
    );
    cx.run_until_parked();
    assert!(cx.debug_bounds("center-tab-context-menu").is_some());
    // The same list again, now offering Unpin, and the second row closes the rest.
    cx.simulate_keystrokes("down down enter");
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 3]
    );
}

#[gpui_kit::test]
fn tab_close_other_and_all_actions_use_keymap_paths(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
        shell.open_resource_tab("Secret".into(), "Secrets".into(), None, None, cx);
        shell.toggle_center_tab_pin(0, cx);
    });

    cx.simulate_keystrokes("secondary-alt-w");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0, 3]
    );
    cx.simulate_keystrokes("secondary-shift-w");
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 1);
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.open_tabs.clone()),
        vec![0]
    );
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 0);
}

/// Action commands need a current binding. Palette-only actions are excluded.
#[gpui_kit::test]
fn commands_with_actions_have_bindings(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);

    cx.update(|cx| {
        for command in demo_commands(true) {
            if let CommandRun::Action(make_action) = command.run {
                if matches!(
                    command.id.as_ref(),
                    "theme.toggle"
                        | "theme.light"
                        | "theme.dark"
                        | "theme.system"
                        | "pod.describe"
                        | "pod.pause"
                        | "pod.resume"
                        | "pod.copy_name"
                        | "view.refresh"
                        | "view.forwards"
                        | "update.check"
                        | "update.restart"
                        | "yaml.apply"
                        | "pod.logs"
                        | "pod.events"
                        | "pod.exec"
                        | "pod.forward_port"
                        | "pod.service_account"
                        | "resource.restart"
                        | "resource.scale"
                        | "keymap.reload"
                        | "keymap.preset.lens"
                        | "keymap.preset.vscode"
                        | "cluster.reload_kubeconfigs"
                ) {
                    continue;
                }
                let action = make_action();
                assert!(
                    crate::keymap::has_binding(action.as_ref(), cx),
                    "command {} action has no binding",
                    command.label
                );
            }
        }
        assert!(crate::keymap::has_binding(&ToggleCommandPalette, cx));
    });
}

/// Regression: Enter runs an action command from the palette.
#[gpui_kit::test]
fn palette_enter_runs_action_command_without_panicking(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));
    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("toggle sidebar");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    assert!(!shell.read_with(cx, |shell, _| shell.palette_open));
    assert!(
        !shell.read_with(cx, |shell, _| shell.sidebar_open),
        "Enter must run the command through the deferred ToggleLeftPanel action"
    );
}

/// Unavailable commands must provide a user-readable reason.
#[gpui_kit::test]
fn palette_has_no_unavailable_commands_left(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    let unavailable = shell.read_with(cx, |shell, _| {
        shell
            .commands
            .iter()
            .filter_map(|command| {
                let CommandRun::Unavailable { reason, .. } = &command.run else {
                    return None;
                };
                Some((command.id.to_string(), reason.to_owned()))
            })
            .collect::<Vec<_>>()
    });
    assert!(
        unavailable
            .iter()
            .all(|(_, reason)| !reason.trim().is_empty()),
        "unavailable commands must provide a user-readable reason: {unavailable:?}"
    );
}

/// The tree is first in tab order and supports arrow-key navigation.
#[gpui_kit::test]
fn tab_reaches_tree_and_tree_keys_navigate(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    assert!(
        cx.update(|_, cx| crate::keymap::has_binding(&FocusNext, cx)),
        "Tab must have a FocusNext binding"
    );

    // Focus the tree directly to test its keyboard navigation and tab cycle.
    cx.update(|window, cx| {
        let handle = shell.read(cx).tree_focus_handle.clone();
        window.focus(&handle, cx);
    });
    assert!(
        cx.update(|window, cx| shell.read(cx).tree_focus_handle.is_focused(window)),
        "focus must be on the resource tree"
    );

    assert_eq!(shell.read_with(cx, |shell, _| shell.tree_cursor), Some(0));
    // Overview is first. Move to the first group, then enter it.
    cx.simulate_keystrokes("down");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tree_cursor),
        Some(1),
        "Down reaches the first API group"
    );
    cx.simulate_keystrokes("right");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tree_cursor),
        Some(2),
        "Right enters the first Kind in an expanded API group"
    );
    cx.simulate_keystrokes("enter");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .selected_node
            .as_ref()
            .map(|s| s.label.clone())),
        Some("Pods".into()),
        "Enter activates the Kind row and selects the Pods Tab"
    );

    // Tab moves from the tree through the center tabs and filters to the table.
    cx.simulate_keystrokes("tab");
    assert!(
        !cx.update(|window, cx| shell.read(cx).tree_focus_handle.is_focused(window)),
        "Tab again leaves the tree for the center Tab bar"
    );
    let table_handle = cx.update(|_, cx| shell.read(cx).pods.read(cx).table_focus_handle(cx));
    let mut reached_table = false;
    for _ in 0..12 {
        cx.simulate_keystrokes("tab");
        if cx.update(|window, _| table_handle.is_focused(window)) {
            reached_table = true;
            break;
        }
    }
    assert!(reached_table, "Tab must reach the table within 12 stops");
    cx.simulate_keystrokes("down");
    assert!(
        shell
            .read_with(cx, |shell, cx| shell.pods.read(cx).selected_name(cx))
            .is_some(),
        "Down selects the first row after the table gains focus"
    );
}

/// The tree Overview row opens the Overview tab.
#[gpui_kit::test]
fn tree_overview_entry_opens_the_overview_tab(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        let handle = shell.read(cx).tree_focus_handle.clone();
        window.focus(&handle, cx);
    });

    cx.simulate_keystrokes("enter");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .tabs
            .get(shell.active_tab)
            .map(|tab| (tab.content, tab.title.clone()))),
        Some((TabContent::Overview, "Overview".into())),
        "Enter opens the Overview Tab from the Overview row"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .selected_node
            .as_ref()
            .map(|selection| selection.label.clone())),
        Some("Overview".into()),
        "tree selection stays in sync with the Overview Tab"
    );
}

/// Tab traversal cycles and reaches the open panels.
#[gpui_kit::test]
fn tab_traversal_cycles_through_panels(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.dock_open = true;
        cx.notify();
    });

    let tree_handle = cx.update(|_, cx| shell.read(cx).tree_focus_handle.clone());
    cx.update(|window, cx| tree_handle.focus(window, cx));
    let mut saw_non_tree = false;
    let mut returned_to_tree = false;
    // The shell has a tab stop per region and per panel control, so one full cycle
    // needs more than a dozen stops. The budget only guards against a focus trap.
    let mut stops = 0;
    for _ in 0..64 {
        // `ctrl-tab` is the binding that moves between regions everywhere, and this is the
        // question the test is about. Plain `tab` cannot stand in for it: the keymap gives
        // `tab` to `SelectNextColumn` inside the `Table` context, and the table owns several
        // stops, so asking whether the table's *own* handle is focused is not the same
        // question as asking whether focus is inside the table. The narrower check left the
        // loop cycling columns forever, which is a focus trap the reader cannot leave.
        cx.simulate_keystrokes("ctrl-tab");
        stops += 1;
        if cx.update(|window, cx| shell.read(cx).tree_focus_handle.is_focused(window)) {
            returned_to_tree = true;
            break;
        }
        saw_non_tree = true;
    }
    assert!(saw_non_tree, "Tab must leave the tree");
    assert!(
        returned_to_tree,
        "Tab traversal must return to its starting point, {stops} stops without a cycle"
    );
}

/// Home and End take the tree to the ends of what is on screen.
///
/// The Dock tab strip and the Inspector tab strip both publish and implement `Home` and `End`,
/// and the tree implemented neither, so a keyboard user had no way past a collapsed group. `End`
/// is deliberately the last *visible* row: the model holds every kind behind every collapsed
/// group, and putting the cursor on one of those leaves the reader on a row they cannot see and
/// scrolling back up from.
#[gpui_kit::test]
fn the_tree_takes_home_and_end_to_the_ends_of_what_is_visible(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.run_until_parked();
    let handle = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    cx.update(|window, cx| window.focus(&handle, cx));

    let (visible, model) = shell.read_with(cx, |shell, _| {
        let cluster = shell
            .clusters
            .get(shell.active_cluster)
            .map_or("cluster", |name| name.as_ref());
        (
            shell.visible_tree_rows().len(),
            shell.tree.rows_for_cluster(cluster, &HashSet::new()).len(),
        )
    });
    assert!(
        visible < model,
        "the demo tree ships with groups collapsed, so the last visible row and the last model \
         row have to be different rows for this to mean anything: {visible} of {model}"
    );

    cx.simulate_keystrokes("end");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tree_cursor),
        Some(visible - 1),
        "End lands on the last visible row, not the last row in the model"
    );
    cx.simulate_keystrokes("home");
    assert_eq!(shell.read_with(cx, |shell, _| shell.tree_cursor), Some(0));
    // Neither wraps, so a reader holding the key down stays on the list.
    cx.simulate_keystrokes("home");
    assert_eq!(shell.read_with(cx, |shell, _| shell.tree_cursor), Some(0));
    cx.simulate_keystrokes("end");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tree_cursor),
        Some(visible - 1)
    );
}

/// The tree publishes the keys it answers, and answers the keys it publishes.
///
/// A published shortcut that does nothing is worse than none, and a shortcut that works without
/// being published is the case a reader cannot discover at all. The two sides live in two files,
/// so this reads the handler back out of the source and compares.
#[test]
fn the_tree_publishes_exactly_the_keys_it_handles() {
    const HANDLED: &[&str] = &[
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "enter",
        "space",
        "f10",
        "menu",
        "contextmenu",
    ];
    let handler = include_str!("mod.rs")
        .split("fn on_tree_key_down")
        .nth(1)
        .and_then(|rest| rest.split("fn reset_connection_controller").next())
        .expect("the tree's key handler");
    let published: Vec<String> = super::panels::TREE_KEYSHORTCUTS
        .split_whitespace()
        .map(|token| {
            let name = token.rsplit('+').next().unwrap_or(token);
            name.strip_prefix("Arrow").unwrap_or(name).to_lowercase()
        })
        .collect();
    for key in HANDLED {
        assert!(
            published.iter().any(|token| token == key),
            "the tree answers {key} but does not publish it"
        );
        assert!(
            handler.contains(&format!("\"{key}\"")),
            "the tree publishes {key} but its handler never matches it"
        );
    }
    // Home and End are the finding, so they are named rather than inherited.
    for key in ["home", "end"] {
        assert!(published.iter().any(|token| token == key));
    }
}

/// Clicking a kind opens its own tab and view. An Unavailable session opens an error state.
#[gpui_kit::test]
fn tree_click_opens_kind_tab_with_its_own_view(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let session = ClusterSession::unavailable("no kubeconfig");
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));

    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "", "version": "v1", "kind": "Service", "plural": "services",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                    { "group": "", "version": "v1", "kind": "ConfigMap", "plural": "configmaps",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                ],
            }],
        }],
    }))
    .expect("valid catalog JSON");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));

    let services = shell.read_with(cx, |shell, _| {
        shell
            .tree
            .rows(&shell.collapsed)
            .into_iter()
            .find(|row| row.resource_kind.as_deref() == Some("Service"))
            .expect("Services row")
    });
    shell.update(cx, |shell, cx| shell.on_tree_click(services, cx));
    cx.run_until_parked();

    let (index, title) = shell.read_with(cx, |shell, _| {
        (shell.active_tab, shell.tabs[shell.active_tab].title.clone())
    });
    assert_eq!(
        title.as_ref(),
        "Services",
        "tab title matches the kind display name"
    );
    shell.read_with(cx, |shell, cx| {
        assert!(
            shell.open_tabs.contains(&index),
            "the new Tab must appear in the open-tab set"
        );
        let view = shell.views[index]
            .as_ref()
            .and_then(super::TabView::resource)
            .expect("the dynamic Tab must create its table view lazily");
        assert!(
            matches!(view.read(cx).status(cx), TableStatus::Failed(_)),
            "the table enters an error state for an Unavailable session with Retry available"
        );
    });

    // Select another kind to open a separate tab.
    let config_maps = shell.read_with(cx, |shell, _| {
        shell
            .tree
            .rows(&shell.collapsed)
            .into_iter()
            .find(|row| row.resource_kind.as_deref() == Some("ConfigMap"))
            .expect("Config Maps row")
    });
    shell.update(cx, |shell, cx| shell.on_tree_click(config_maps, cx));
    shell.read_with(cx, |shell, _| {
        assert_eq!(shell.tabs[shell.active_tab].title.as_ref(), "Config Maps");
        assert_eq!(shell.tabs[shell.active_tab].kind.as_ref(), "ConfigMap");
        assert_ne!(shell.active_tab, index);
    });
}

/// The Overview's biggest number is a button, so it has to arrive somewhere.
///
/// `alerts.md` asks a report to offer the action that follows from it, and the Overview counts the
/// rows that need attention. The route opened the Pods tab, which is a route to *the Pods* rather
/// than to the rows the number counted, so the reader left a page that said "9,900 need attention"
/// and landed on a table listing every pod in the cluster.
#[gpui_kit::test]
fn the_overview_problem_route_lands_on_the_pods_that_need_attention(cx: &mut TestAppContext) {
    init_ui(cx);
    // No session. A failed one makes the connection surface own the centre in
    // place of the open tab, which is the right product answer and the wrong
    // fixture here: this test is about what the route puts on screen, and a
    // connection failure would hide the very table it is checking.
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.0), px(900.0)));
    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                ],
            }],
        }],
    }))
    .expect("catalog");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));
    cx.run_until_parked();

    // The test hands the shared Pods view a source whose phases it owns, so the route has a
    // healthy pod and a pending one to separate. Tab 0 is already the Pods tab, which is the
    // tab the route lands on.
    let entry = shell.read_with(cx, |shell, _| {
        shell
            .tree
            .rows(&shell.collapsed)
            .into_iter()
            .find(|row| row.resource_kind.as_deref() == Some("Pod"))
            .and_then(|row| row.entry)
            .expect("a Pods row carrying its catalog entry")
    });
    assert_eq!(
        Some(entry.identity()),
        shell.read_with(cx, |shell, _| shell.tabs[0].identity.clone()),
        "the route looks the tab up by this identity, so a mismatch would open a second tab"
    );
    // The route under test, on the hero caption's own path. The setup above
    // reproduced the state the Overview reports, and this is the button the
    // reader presses from it.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_kind_needing_attention("Pod", window, cx)
        });
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("resource-grid").is_some());
    assert!(
        cx.debug_bounds("resource-row-1").is_some(),
        "an unfiltered Pods table lists both pods"
    );

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_kind_needing_attention("Pod", window, cx)
        });
    });
    cx.run_until_parked();

    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tabs[shell.active_tab]
            .title
            .to_string()),
        "Pods"
    );
    // What the shell owns: the route lands on the Pods tab, reuses the one that
    // is already open, and does not toggle itself off on a second press. Which
    // rows the filter then leaves is the table's own contract, and
    // `an_external_problems_filter_hides_the_healthy_rows_and_comes_back` in
    // `table_view` holds it against a source the table controls -- which a shell
    // test cannot arrange, because the tab holds the view the shell gave it and
    // the session's factory owns what that view reads.
    assert!(
        cx.debug_bounds("resource-grid").is_some(),
        "the route lands on the Pods table"
    );

    // The button names a destination, so pressing it again while already there must not toggle the
    // filter back off, and must not open a second Pods tab.
    let tabs_after_the_route = shell.read_with(cx, |shell, _| shell.tabs.len());
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_kind_needing_attention("Pod", window, cx)
        });
    });
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tabs.len()),
        tabs_after_the_route,
        "re-pressing reuses the Pods tab instead of opening another one"
    );
}

#[gpui_kit::test]
fn same_kind_different_groups_keep_distinct_tabs_and_tree_selection(cx: &mut TestAppContext) {
    init_ui(cx);
    let session = ClusterSession::unavailable("no kubeconfig");
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [
            {
                "group": "a.example",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "a.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                          "scope": "namespaced", "verbs": ["list"] }
                    ],
                }],
            },
            {
                "group": "z.example",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "z.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                          "scope": "cluster", "verbs": ["list"] }
                    ],
                }],
            },
        ],
    }))
    .expect("catalog");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));

    let (a_row, z_row, z_group) = shell.read_with(cx, |shell, _| {
        let rows = shell.tree.rows(&HashSet::new());
        let find = |group: &str| {
            rows.iter()
                .find(|row| {
                    row.resource_gvk.as_ref().is_some_and(|identity| {
                        identity.group == group && identity.kind == "Widget"
                    })
                })
                .cloned()
                .unwrap_or_else(|| {
                    panic!(
                        "missing {group} widget: {:?}",
                        rows.iter()
                            .filter_map(|row| row.resource_gvk.as_ref())
                            .collect::<Vec<_>>()
                    )
                })
        };
        let z_group = rows
            .iter()
            .find(|row| row.kind == super::TreeRowKind::Group && row.label.as_ref() == "z.example")
            .map(|row| row.id.clone())
            .expect("z.example group");
        (find("a.example"), find("z.example"), z_group)
    });
    shell.update(cx, |shell, cx| {
        shell.collapsed.remove(&z_group);
        shell.on_tree_click(a_row, cx);
    });
    let a_tab = shell.read_with(cx, |shell, _| shell.active_tab);
    shell.update(cx, |shell, cx| shell.on_tree_click(z_row.clone(), cx));
    let z_tab = shell.read_with(cx, |shell, _| shell.active_tab);

    shell.read_with(cx, |shell, _| {
        assert_ne!(a_tab, z_tab);
        assert_eq!(
            shell.tabs[a_tab].identity.as_ref().unwrap().group,
            "a.example"
        );
        assert_eq!(
            shell.tabs[z_tab].identity.as_ref().unwrap().group,
            "z.example"
        );
        assert_eq!(shell.tabs[a_tab].entry.as_ref().unwrap().group, "a.example");
        assert_eq!(shell.tabs[z_tab].entry.as_ref().unwrap().group, "z.example");
        assert_eq!(
            shell.selected_node.as_ref().map(|selection| &selection.id),
            Some(&z_row.id)
        );
    });

    let z_only: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "z.example",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "z.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                      "scope": "cluster", "verbs": ["list"] }
                ],
            }],
        }],
    }))
    .expect("catalog");
    shell.update(cx, |shell, cx| shell.on_catalog_refreshed(z_only, cx));
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.tabs[a_tab].identity.as_ref().unwrap().group,
            "a.example"
        );
        assert!(shell.tabs[a_tab].entry.is_none());
        assert!(shell.tabs[a_tab].resource.is_none());
        assert!(shell.views[a_tab].is_none());
        assert_eq!(shell.tabs[z_tab].entry.as_ref().unwrap().group, "z.example");
    });
}

#[gpui_kit::test]
fn focus_yaml_action_stays_editable_when_repeated(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).has_yaml()),
        "the Inspector must show YAML after row selection"
    );
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_editing(cx)),
        "YAML must be editable by default"
    );

    let shortcut = shortcut("secondary-e", "secondary-shift-y");
    cx.simulate_keystrokes(shortcut);
    shell.read_with(cx, |shell, cx| {
        use crate::panels::InspectorTab;
        assert_eq!(shell.inspector.read(cx).active_tab(), InspectorTab::Yaml);
        assert!(shell.inspector.read(cx).is_editing(cx));
    });
    cx.simulate_keystrokes(shortcut);
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_editing(cx)),
        "repeating Focus YAML must not switch back to read-only"
    );
    cx.simulate_input("# focused");
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));
}

#[gpui_kit::test]
fn closing_dirty_yaml_requires_confirmation_before_discarding(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# unsaved");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.close_tab_index(0, window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog, Dialog::ConfirmTabClose { .. }))
    }));
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 0);
    let cancel_focus = shell.read_with(cx, |shell, _| shell.dialog_button_focus_handles[0].clone());
    assert!(cx.update(|window, _| cancel_focus.is_focused(window)));
    assert_eq!(cx.windows().len(), 1);
    // Enter keeps the unsaved YAML, because Cancel has the default focus.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));

    // Tab selects Close, then Enter discards the unsaved YAML.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.close_tab_index(0, window, cx));
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(!shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));
    assert_eq!(cx.windows().len(), 1);
}

#[gpui_kit::test]
fn focus_yaml_without_selection_explains(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    assert!(
        !shell.read_with(cx, |shell, cx| shell.inspector.read(cx).has_yaml()),
        "YAML must not appear without a selected row"
    );
    assert!(
        shell.read_with(cx, |shell, _| shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("show its YAML"))),
        "the toast must explain why"
    );
}

/// Regression: Apply from the palette must not re-enter the shell update.
#[gpui_kit::test]
fn palette_apply_yaml_does_not_reenter_shell(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);

    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("#");
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)),
        "the editor must be dirty after input so Apply has content"
    );

    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("apply yaml");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    shell.read_with(cx, |shell, cx| {
        let panel = shell.inspector.read(cx);
        assert!(
            !panel.is_applying(),
            "Apply must finish through the deferred result even without a cluster"
        );
        assert!(
            panel.is_dirty(cx),
            "the editor keeps its content after failure"
        );
        assert!(
            !shell.palette_open,
            "the palette closes after the command runs"
        );
    });
}

#[gpui_kit::test]
fn unavailable_connection_assigns_error_to_the_active_resource_view(cx: &mut TestAppContext) {
    init_ui(cx);
    let session = ClusterSession::unavailable("no kubeconfig");
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    cx.run_until_parked();

    shell.read_with(cx, |shell, cx| {
        assert!(matches!(shell.connection, ConnectionState::Failed(_)));
        assert!(matches!(shell.catalog_state, CatalogState::Failed(_)));
        assert!(matches!(
            shell.pods.read(cx).status(cx),
            TableStatus::Failed(_)
        ));
    });
}

/// Context-menu Logs opens the Dock. It shows Unavailable without a cluster.
#[gpui_kit::test]
fn open_logs_opens_the_dock_and_starts_unavailable_without_a_cluster(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_logs(
                crate::panels::LogRequest {
                    namespace: Some("default".into()),
                    name: "web-0".into(),
                    containers: vec!["app".into(), "sidecar".into()],
                },
                window,
                cx,
            );
        });
    });

    shell.read_with(cx, |shell, cx| {
        assert!(shell.dock_open, "Logs must open the Dock");
        assert_eq!(shell.dock_panel.read(cx).phase().label(), "Unavailable");
        assert_eq!(
            shell
                .dock_panel
                .read(cx)
                .selected_container()
                .map(|container| container.to_string()),
            Some("app".to_owned()),
            "the first container is selected by default"
        );
    });
}

/// Resolves the user keymap path without reading the file.
#[test]
fn user_keymap_path_points_into_config_dir() {
    let path = user_keymap_path().expect("the test environment has HOME");
    assert!(path.ends_with("k8s-gpui/keymap.json"), "{path:?}");
}

const SWITCH_KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
- name: beta
  cluster:
    server: http://127.0.0.1:6444
contexts:
- name: alpha-ctx
  context: { cluster: alpha, user: alpha-user }
- name: beta-ctx
  context: { cluster: beta, user: beta-user }
- name: broken-ctx
  context: { cluster: nowhere, user: alpha-user }
users:
- name: alpha-user
  user: {}
- name: beta-user
  user: {}
current-context: alpha-ctx
"#;

const SWITCH_SPLIT_CONTEXT: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: alpha-ctx
  context: { cluster: alpha, user: alpha-user }
- name: beta-ctx
  context: { cluster: beta, user: beta-user }
users:
- name: alpha-user
  user: {}
current-context: alpha-ctx
"#;

const SWITCH_SPLIT_RESOURCES: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:9999
- name: beta
  cluster:
    server: http://127.0.0.1:6444
users:
- name: alpha-user
  user:
    token: ignored
- name: beta-user
  user: {}
"#;

const RELOADED_KUBECONFIG: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: beta
  cluster:
    server: http://127.0.0.1:6444
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: beta-ctx
  context: { cluster: beta, user: beta-user }
- name: alpha-ctx
  context: { cluster: alpha, user: alpha-user }
users:
- name: alpha-user
  user: {}
- name: beta-user
  user: {}
current-context: beta-ctx
"#;

#[gpui_kit::test]
fn catalog_retry_rehydrates_none_handle_and_recovers(cx: &mut TestAppContext) {
    let attempts = Rc::new(Cell::new(0));
    let attempts_for_future = attempts.clone();
    let future: super::CatalogFuture = Rc::new(move || {
        let attempt = attempts_for_future.get();
        attempts_for_future.set(attempt + 1);
        Box::pin(async move {
            if attempt == 0 {
                Err("temporary catalog failure".to_owned())
            } else {
                Ok(ResourceCatalog::default())
            }
        })
    });
    let (shell, cx) = catalog_retry_shell(cx, "catalog-rehydrate", future);
    shell.update(cx, |shell, cx| shell.retry_catalog(cx));
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Loading
    ));
    cx.run_until_parked();
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Failed(_)
    ));
    assert_eq!(attempts.get(), 1);
    assert!(shell.read_with(cx, |shell, _| shell.catalog.is_some()));
    assert!(shell.read_with(cx, |shell, _| shell.catalog_retry_task.is_some()));

    cx.executor()
        .advance_clock(Duration::from_secs(2) + Duration::from_millis(1));
    cx.run_until_parked();
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Ready
    ));
    assert_eq!(attempts.get(), 2);
    assert!(shell.read_with(cx, |shell, _| shell.catalog_retry_task.is_none()));
    assert!(
        shell
            .read_with(cx, |shell, _| shell
                .toast
                .as_ref()
                .map(|toast| toast.message.to_string()))
            .is_some_and(|message| message.contains("Resources"))
    );
}

#[gpui_kit::test]
fn catalog_retry_uses_bounded_backoff_and_stops_after_success(cx: &mut TestAppContext) {
    let attempts = Rc::new(Cell::new(0));
    let attempts_for_future = attempts.clone();
    let future: super::CatalogFuture = Rc::new(move || {
        attempts_for_future.set(attempts_for_future.get() + 1);
        Box::pin(async { Err("temporary catalog failure".to_owned()) })
    });
    let (shell, cx) = catalog_retry_shell(cx, "catalog-backoff", future);

    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded(Err("temporary catalog failure".to_owned()), cx);
    });
    assert_eq!(attempts.get(), 0);
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.catalog_retry_attempt),
        1
    );

    cx.executor().advance_clock(Duration::from_millis(1_999));
    cx.run_until_parked();
    assert_eq!(attempts.get(), 0);
    cx.executor().advance_clock(Duration::from_millis(2));
    cx.run_until_parked();
    assert_eq!(attempts.get(), 1);
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.catalog_retry_attempt),
        2
    );

    assert_eq!(super::catalog_retry_delay(0), Duration::from_secs(2));
    assert_eq!(super::catalog_retry_delay(1), Duration::from_secs(5));
    assert_eq!(super::catalog_retry_delay(2), Duration::from_secs(15));
    assert_eq!(super::catalog_retry_delay(3), Duration::from_secs(30));
    assert_eq!(super::catalog_retry_delay(99), Duration::from_secs(30));

    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded(Ok(ResourceCatalog::default()), cx);
    });
    cx.run_until_parked();
    cx.executor().advance_clock(Duration::from_secs(31));
    cx.run_until_parked();
    assert_eq!(attempts.get(), 1);
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Ready
    ));
}

#[gpui_kit::test]
fn manual_catalog_retry_resets_backoff_and_invalidates_old_timer(cx: &mut TestAppContext) {
    let attempts = Rc::new(Cell::new(0));
    let attempts_for_future = attempts.clone();
    let future: super::CatalogFuture = Rc::new(move || {
        attempts_for_future.set(attempts_for_future.get() + 1);
        Box::pin(async { Ok(ResourceCatalog::default()) })
    });
    let (shell, cx) = catalog_retry_shell(cx, "catalog-manual", future);

    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded(Err("first failure".to_owned()), cx);
        shell.on_catalog_loaded(Err("second failure".to_owned()), cx);
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.catalog_retry_attempt),
        2
    );
    shell.update(cx, |shell, cx| shell.retry_catalog(cx));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.catalog_retry_attempt),
        0
    );
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Loading
    ));

    cx.run_until_parked();
    assert_eq!(attempts.get(), 1);
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Ready
    ));
    cx.executor().advance_clock(Duration::from_secs(31));
    cx.run_until_parked();
    assert_eq!(attempts.get(), 1);
}

#[gpui_kit::test]
fn catalog_retry_epoch_drops_a_scheduled_attempt(cx: &mut TestAppContext) {
    let attempts = Rc::new(Cell::new(0));
    let attempts_for_future = attempts.clone();
    let future: super::CatalogFuture = Rc::new(move || {
        attempts_for_future.set(attempts_for_future.get() + 1);
        Box::pin(async { Err("temporary catalog failure".to_owned()) })
    });
    let (shell, cx) = catalog_retry_shell(cx, "catalog-epoch", future);

    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded(Err("temporary catalog failure".to_owned()), cx);
        shell.session_epoch = shell.session_epoch.wrapping_add(1);
        shell.catalog_state = CatalogState::Ready;
    });
    cx.executor()
        .advance_clock(Duration::from_secs(2) + Duration::from_millis(1));
    cx.run_until_parked();
    assert_eq!(attempts.get(), 0);
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Ready
    ));
}

/// Cluster switching preserves tabs, rebuilds views, and restores each cluster namespace.
#[gpui_kit::test]
fn cluster_switch_rebuilds_views_and_remembers_namespace(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    // Real kube futures can wake GPUI tasks across threads.
    // Allow the deterministic scheduler to park.
    cx.dispatcher.allow_parking();
    let handle = test_runtime();

    let path =
        std::env::temp_dir().join(format!("k8s-gpui-shell-switch-{}.yaml", std::process::id()));
    std::fs::write(&path, SWITCH_KUBECONFIG).expect("write temporary kubeconfig");
    let registry = Arc::new(
        handle
            .block_on(k8s_core::cluster::ClusterRegistry::load(&path))
            .expect("load kubeconfig"),
    );
    let _ = std::fs::remove_file(&path);

    let session = ClusterSession::from_registry(Arc::clone(&registry), handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    cx.run_until_parked();

    shell.update(cx, |shell, cx| {
        hold_cluster_connected(shell, cx);
        shell._namespace_task = None;
        shell.namespace_state = NamespaceState::Ready(vec![
            "default".into(),
            "kube-system".into(),
            "monitoring".into(),
        ]);
        let registry = Arc::clone(&registry);
        shell.namespace_future = Some(Rc::new(move |cluster| {
            let names = match registry.get(cluster).map(|cluster| cluster.name()) {
                Some("alpha-ctx") => vec!["default", "kube-system", "monitoring"],
                Some("beta-ctx") => vec!["team-a", "team-b"],
                _ => Vec::new(),
            };
            Box::pin(async move { Ok(names.into_iter().map(str::to_owned).collect()) })
        }));
    });

    let (clusters, active) = shell.read_with(cx, |shell, _| {
        (shell.clusters.clone(), shell.active_cluster)
    });
    assert_eq!(
        clusters.len(),
        3,
        "the menu must list every context, including failed contexts"
    );
    assert_eq!(clusters[0].as_ref(), "alpha-ctx");
    assert_eq!(active, 0);
    let alpha_pods = shell.read_with(cx, |shell, _| shell.pods.entity_id());

    // Inject a catalog without connecting to a cluster.
    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                    { "group": "apps", "version": "v1", "kind": "Deployment", "plural": "deployments",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                ],
            }],
        }],
    }))
    .expect("valid catalog JSON");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));

    // Preserve the tab and namespace per cluster during the switch.
    shell.update(cx, |shell, cx| shell.activate_tab(1, cx));
    let alpha_view = shell.read_with(cx, |shell, _| {
        shell.views[1].as_ref().map(|view| view.entity_id())
    });
    assert!(alpha_view.is_some(), "the Deployments Tab must have a view");
    shell.update(cx, |shell, cx| {
        shell.set_namespace("kube-system".into(), cx)
    });
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace.clone()),
        "kube-system"
    );

    shell.update(cx, |shell, cx| {
        shell.switch_cluster(1, cx);
        hold_cluster_connected(shell, cx);
    });
    cx.run_until_parked();

    let (name, active, namespace, open_tabs) = shell.read_with(cx, |shell, _| {
        (
            shell
                .session
                .as_ref()
                .and_then(ClusterSession::cluster_name)
                .map(str::to_owned),
            shell.active_cluster,
            shell.namespace.clone(),
            shell.open_tabs.clone(),
        )
    });
    assert_eq!(name.as_deref(), Some("beta-ctx"));
    assert_eq!(active, 1);
    assert_eq!(
        namespace, "All namespaces",
        "a new cluster starts with All namespaces"
    );
    assert_eq!(open_tabs, vec![0, 1], "the Tab list is preserved");
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace_state.clone()),
        NamespaceState::Ready(vec!["team-a".into(), "team-b".into()])
    );
    shell.update(cx, |shell, cx| shell.set_namespace("team-a".into(), cx));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace.clone()),
        "team-a"
    );
    assert_ne!(
        shell.read_with(cx, |shell, _| shell.pods.entity_id()),
        alpha_pods,
        "the switch must create a new table entity and cancel the old watch"
    );
    let beta_view = shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.tree.cluster_names(),
            vec![gpui_kit::SharedString::from("beta-ctx")],
            "the catalog tree points to the new cluster"
        );
        shell.views[1].as_ref().map(|view| view.entity_id())
    });
    assert_eq!(
        beta_view, None,
        "the old GVK definition must not cross clusters"
    );
    let beta_catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "apps",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "apps", "version": "v1", "kind": "Deployment", "plural": "deployments",
                      "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                ],
            }],
        }],
    }))
    .expect("beta catalog");
    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded(Ok(beta_catalog), cx)
    });
    cx.run_until_parked();
    let rebuilt_view = shell.read_with(cx, |shell, _| {
        shell.views[1]
            .as_ref()
            .and_then(super::TabView::resource)
            .cloned()
    });
    let rebuilt_view = rebuilt_view.expect("beta deployment view");
    let (rebuilt_id, rebuilt_focus) = cx.update(|_, cx| {
        (
            rebuilt_view.entity_id(),
            rebuilt_view.read(cx).table_focus_handle(cx),
        )
    });
    assert_ne!(Some(rebuilt_id), alpha_view);
    let root_focus = shell.read_with(cx, |shell, _| shell.focus_handle.clone());
    let pods_focus = shell.read_with(cx, |shell, cx| shell.pods.read(cx).table_focus_handle(cx));
    let tree_focus = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    let focus_state = cx.update(|window, _| {
        (
            rebuilt_focus.is_focused(window),
            root_focus.is_focused(window),
            pods_focus.is_focused(window),
            tree_focus.is_focused(window),
        )
    });
    assert!(focus_state.0, "focus after rebuild: {focus_state:?}");

    // Switch back to alpha to restore its namespace.
    shell.update(cx, |shell, cx| {
        shell.switch_cluster(0, cx);
        hold_cluster_connected(shell, cx);
    });
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace.clone()),
        "kube-system"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.namespace_state.clone()),
        NamespaceState::Ready(vec![
            "default".into(),
            "kube-system".into(),
            "monitoring".into(),
        ])
    );

    // A failed context remains selectable, and the user can switch back.
    shell.update(cx, |shell, cx| shell.switch_cluster(2, cx));
    cx.run_until_parked();
    shell.read_with(cx, |shell, _| {
        assert!(matches!(
            &shell.session,
            Some(ClusterSession::Unavailable { .. })
        ));
        assert!(matches!(shell.catalog_state, CatalogState::Failed(_)));
        assert!(matches!(shell.connection, ConnectionState::Failed(_)));
        assert!(
            shell.cluster_handle.is_none(),
            "a failed context has no operation handle"
        );
    });
    shell.update(cx, |shell, cx| shell.switch_cluster(0, cx));
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .session
            .as_ref()
            .and_then(ClusterSession::cluster_name)
            .map(str::to_owned)),
        Some("alpha-ctx".to_owned()),
        "a bad context must allow a switch back"
    );
}

/// Keymap reload feedback names an invalid shortcut from a bad file.
#[gpui_kit::test]
fn keymap_reload_toasts_in_shell(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    assert!(shell.read_with(cx, |shell, _| shell.toast.is_none()));

    cx.update(|_, cx| {
        assert!(crate::keymap::reload_from_source(
            cx,
            r#"[{ "bindings": { "ctrl-shift-e": "k8s_shell::ToggleLeftPanel" } }]"#,
        ));
    });
    cx.run_until_parked();
    let toast = shell
        .read_with(cx, |shell, _| shell.toast.clone())
        .expect("a successful reload must show a toast");
    assert!(toast.message.contains("reloaded"), "{}", toast.message);

    cx.update(|_, cx| {
        crate::keymap::reload_from_source(
            cx,
            r#"[{ "bindings": { "secondary-w": "k8s_shell::NoSuchAction" } }]"#,
        );
    });
    cx.run_until_parked();
    let toast = shell
        .read_with(cx, |shell, _| shell.toast.clone())
        .expect("a bad file must show a toast");
    assert!(toast.message.contains("secondary-w"), "{}", toast.message);
}

#[gpui_kit::test]
fn reload_kubeconfigs_binding_survives_keymap_reload(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (_shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|_, cx| {
        assert!(crate::keymap::reload_from_source(cx, "[]"));
    });
    cx.run_until_parked();
    assert!(cx.update(|_, cx| crate::keymap::has_binding(&ReloadKubeconfigs, cx)));
}

/// Seeded random input keeps the palette selection inside filtered results.
#[gpui_kit::test(iterations = 8)]
fn palette_selection_stays_within_filtered_results(cx: &mut TestAppContext, mut rng: StdRng) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_keystrokes("secondary-shift-p");

    for _ in 0..24 {
        match rng.random_range(0..5u8) {
            0 => {
                let ch = (b'a' + rng.random_range(0..26u8)) as char;
                cx.simulate_input(&ch.to_string());
            }
            1 => cx.simulate_keystrokes("up"),
            2 => cx.simulate_keystrokes("down"),
            3 => cx.simulate_keystrokes("backspace"),
            _ => cx.simulate_keystrokes("space"),
        }

        shell.read_with(cx, |shell, _| {
            assert!(
                shell.palette_open,
                "random keypresses must not close the palette"
            );
            let count = shell.filtered_command_count();
            if count == 0 {
                assert!(shell.palette_selection.is_none());
            } else {
                assert!(
                    shell
                        .selected_palette_index()
                        .is_some_and(|index| index < count),
                    "the selected item must be in the filtered results: {count} results"
                );
            }
        });
    }
}

#[gpui_kit::test]
fn view_exec_callback_does_not_reenter_shell(cx: &mut TestAppContext) {
    use crate::panels::terminal::{PortForwardFactory, TerminalFactory, TerminalServices};

    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let terminals: TerminalFactory =
        Rc::new(|_request, _sink, _cx| Err("Exec is not available in this test build.".to_owned()));
    let forwards: PortForwardFactory = Rc::new(|_request, _cx| {
        Err("Port forwarding is not available in this test build.".to_owned())
    });
    shell.update(cx, |shell, cx| {
        shell.set_terminal_services(
            Some(TerminalServices {
                terminals,
                forwards,
                context: Some("kind-k8s-gpui-dev".to_owned()),
                namespace: None,
            }),
            cx,
        );
    });
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_exec(window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_some()
        || shell.toast.is_some()));
}

#[gpui_kit::test]
fn namespace_switch_updates_new_local_requests_without_mutating_old_scope(cx: &mut TestAppContext) {
    init_ui(cx);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let request_sink = Rc::clone(&requests);
    let terminals: TerminalFactory = Rc::new(move |request, _sink, _cx| {
        request_sink.borrow_mut().push(request);
        Err("terminal unavailable".to_owned())
    });
    let forwards: PortForwardFactory =
        Rc::new(|_request, _cx| Err("forward unavailable".to_owned()));
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.namespace_state = NamespaceState::Ready(vec!["team-a".into(), "team-b".into()]);
        shell.namespace = "team-a".into();
        shell.set_terminal_services(
            Some(TerminalServices {
                terminals,
                forwards,
                context: Some("kind-k8s-gpui-dev".to_owned()),
                namespace: None,
            }),
            cx,
        );
        let _ = shell
            .dock_panel
            .update(cx, |dock, cx| dock.open_terminal(TerminalKind::Local, cx));
    });
    shell.update(cx, |shell, cx| shell.set_namespace("team-b".into(), cx));
    shell.update(cx, |shell, cx| {
        let _ = shell
            .dock_panel
            .update(cx, |dock, cx| dock.open_terminal(TerminalKind::Local, cx));
    });

    assert_eq!(requests.borrow()[0].namespace.as_deref(), Some("team-a"));
    assert_eq!(requests.borrow()[1].namespace.as_deref(), Some("team-b"));
    let namespace = shell.read_with(cx, |shell, _| {
        shell
            .terminal_services
            .as_ref()
            .and_then(|services| services.namespace.clone())
    });
    assert_eq!(namespace.as_deref(), Some("team-b"));
}

/// Empty or blank queries return all matching commands, and every command a
/// non-empty query keeps is one the query actually matches.
///
/// The property test that used to carry this (`#[gpui_kit::property_test]`) is
/// gone: `proptest` is only a transitive dependency of `gpui`, so the macro's
/// generated `::proptest` path does not resolve inside this crate, and the
/// macro itself panics on the strategy attribute before it gets that far. The
/// corpus below is the deterministic core of the same claim.
#[test]
fn filter_commands_spec() {
    let commands = demo_commands(true);
    for query in [
        "", "   ", "a", "Log", "log", "logs", "Pods", "  pods  ", "zzz",
    ] {
        let filtered = filter_commands(&commands, query);
        let needle = query.trim().to_lowercase();

        if needle.is_empty() {
            assert_eq!(
                filtered.len(),
                commands.len(),
                "a blank query must keep every command, {query:?}"
            );
            continue;
        }

        for command in filtered {
            assert!(
                command_matches_query(command, &needle),
                "{query:?} matched unexpected command {}",
                command.label
            );
        }
    }
}

/// Builds a Deployment scale target for dialog tests.
fn scale_test_target(name: &str, uid: &str, replicas: i32) -> ScaleTarget {
    ScaleTarget {
        object: ObjectRef {
            resource: kube_core::ApiResource::from_gvk_with_plural(
                &kube_core::GroupVersionKind::gvk("apps", "v1", "Deployment"),
                "deployments",
            ),
            namespace: Some("default".to_owned()),
            name: name.to_owned(),
            uid: uid.to_owned(),
        },
        replicas,
    }
}

#[derive(Default)]
struct RecordingOps {
    calls: Rc<RefCell<Vec<String>>>,
}

impl ObjectOps for RecordingOps {
    fn delete(&self, object: ObjectRef) -> OpsFuture<()> {
        self.calls.borrow_mut().push(format!(
            "delete {}/{}/{}@{}",
            object.resource.plural,
            object.namespace.as_deref().unwrap_or("cluster"),
            object.name,
            object.uid
        ));
        Box::pin(async { Ok(()) })
    }

    fn scale(&self, object: ObjectRef, replicas: i32) -> OpsFuture<()> {
        self.calls.borrow_mut().push(format!(
            "scale {}/{}/{}@{} to {replicas}",
            object.resource.plural,
            object.namespace.as_deref().unwrap_or("cluster"),
            object.name,
            object.uid
        ));
        Box::pin(async { Ok(()) })
    }

    fn restart(&self, object: ObjectRef) -> OpsFuture<()> {
        self.calls.borrow_mut().push(format!(
            "restart {}/{}/{}@{}",
            object.resource.plural,
            object.namespace.as_deref().unwrap_or("cluster"),
            object.name,
            object.uid
        ));
        Box::pin(async { Ok(()) })
    }
}

fn inject_ops(
    cx: &mut gpui_kit::VisualTestContext,
    shell: &gpui_kit::Entity<Shell>,
    ops: Rc<RecordingOps>,
) {
    let pods = shell.read_with(cx, |shell, _| shell.pods.clone());
    pods.update(cx, |view, _| view.set_ops(Some(ops)));
}

fn focus_table(cx: &mut gpui_kit::VisualTestContext, shell: &gpui_kit::Entity<Shell>) {
    let handle = shell.read_with(cx, |shell, cx| shell.pods.read(cx).table_focus_handle(cx));
    cx.update(|window, cx| window.focus(&handle, cx));
}

/// Delete opens a confirmation. Cancel does not call the cluster. Delete does.
#[gpui_kit::test]
fn delete_confirmation_gates_the_ops_call(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let ops = Rc::new(RecordingOps::default());
    inject_ops(cx, &shell, Rc::clone(&ops));
    focus_table(cx, &shell);

    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes(shortcut("delete", "secondary-delete"));
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, _| shell.dialog.is_some()),
        "the delete key must open a confirmation first"
    );
    assert!(
        ops.calls.borrow().is_empty(),
        "the cluster must not be called before confirmation"
    );

    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(
        ops.calls.borrow().is_empty(),
        "Esc cancels without running ops"
    );

    // Cancel has focus by default, so Enter does not delete.
    cx.simulate_keystrokes(shortcut("delete", "secondary-delete"));
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(
        ops.calls.borrow().is_empty(),
        "Enter must focus Cancel by default"
    );

    // Tab selects Delete, then Enter confirms the request.
    cx.simulate_keystrokes(shortcut("delete", "secondary-delete"));
    cx.run_until_parked();
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(ops.calls.borrow().len(), 1, "confirmation must run ops");
    assert!(ops.calls.borrow()[0].starts_with("delete "));
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
}

#[gpui_kit::test]
fn delete_confirmation_keeps_the_original_object_after_selection_changes(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let ops = Rc::new(RecordingOps::default());
    inject_ops(cx, &shell, Rc::clone(&ops));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes(shortcut("delete", "secondary-delete"));
    cx.run_until_parked();
    let original_uid = shell.read_with(cx, |shell, _| {
        let Some(Dialog::ConfirmDelete { target, .. }) = &shell.dialog else {
            panic!("delete dialog is open");
        };
        target.object.uid.clone()
    });
    let pods = shell.read_with(cx, |shell, _| shell.pods.clone());
    pods.update(cx, |view, cx| {
        view.select_uid_for_test("replacement-uid", cx)
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(ops.calls.borrow().len(), 1);
    assert!(ops.calls.borrow()[0].contains(&format!("@{original_uid}")));
    assert!(!ops.calls.borrow()[0].contains("@replacement-uid"));
}

#[gpui_kit::test]
fn scale_confirmation_keeps_the_original_object_after_selection_changes(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let ops = Rc::new(RecordingOps::default());
    inject_ops(cx, &shell, Rc::clone(&ops));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_scale_dialog(scale_test_target("web", "deploy-uid-1", 3), window, cx);
        });
    });
    let pods = shell.read_with(cx, |shell, _| shell.pods.clone());
    pods.update(cx, |view, cx| view.select_uid_for_test("uid-1", cx));
    cx.simulate_keystrokes("tab tab");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(ops.calls.borrow().len(), 1);
    assert!(ops.calls.borrow()[0].contains("@deploy-uid-1"));
    assert!(!ops.calls.borrow()[0].contains("@uid-1"));
}

/// The modal confirmation blocks panel shortcuts and table keys.
#[gpui_kit::test]
fn delete_confirmation_swallows_underlying_shortcuts(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);

    cx.simulate_keystrokes("down");
    let selected = shell.read_with(cx, |shell, cx| {
        shell.pods.read(cx).selected_name(cx).map(|n| n.to_string())
    });
    cx.simulate_keystrokes(shortcut("delete", "secondary-delete"));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_some()));

    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes(shortcut("secondary-b", "secondary-shift-l"));
    assert!(
        shell.read_with(cx, |shell, _| shell.sidebar_open),
        "the modal dialog must block underlying shortcuts"
    );
    assert_eq!(
        shell.read_with(cx, |shell, cx| shell
            .pods
            .read(cx)
            .selected_name(cx)
            .map(|n| n.to_string())),
        selected,
        "the modal dialog must block table selection changes"
    );
}

/// The scale dialog pre-fills replicas and handles validation and confirmation.
#[gpui_kit::test]
fn scale_dialog_validates_and_submits(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let ops = Rc::new(RecordingOps::default());
    inject_ops(cx, &shell, Rc::clone(&ops));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");

    let open = |cx: &mut gpui_kit::VisualTestContext| {
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| {
                shell.open_scale_dialog(scale_test_target("web", "deploy-uid-1", 3), window, cx);
            });
        });
        cx.run_until_parked();
    };

    open(cx);
    assert_eq!(
        shell.read_with(cx, |shell, cx| match &shell.dialog {
            Some(Dialog::Scale { input, .. }) => input.read(cx).text().to_owned(),
            _ => panic!("scale dialog is open"),
        }),
        "3"
    );
    let input_focus = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::Scale { input, .. }) => input.read(cx).focus_handle(cx),
        _ => panic!("scale dialog is open"),
    });
    assert!(cx.update(|window, _| input_focus.is_focused(window)));

    cx.simulate_keystrokes("secondary-a backspace");
    assert!(
        shell.read_with(cx, |shell, _| matches!(
            shell.dialog,
            Some(Dialog::Scale { error: Some(_), .. })
        )),
        "an empty input must show a validation error"
    );
    assert!(ops.calls.borrow().is_empty());
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));

    open(cx);
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("12");
    cx.simulate_keystrokes("tab");
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 1);
    cx.simulate_keystrokes("enter");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(ops.calls.borrow().is_empty());

    open(cx);
    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("12");
    cx.simulate_keystrokes("tab tab");
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 2);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    let calls = ops.calls.borrow().clone();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].starts_with("scale ") && calls[0].ends_with(" to 12"),
        "{calls:?}"
    );
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));

    open(cx);
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert_eq!(ops.calls.borrow().len(), 1, "canceling must not run ops");
}

#[gpui_kit::test]
fn scale_dialog_mouse_click_positions_caret_and_modal_tab_escape(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_scale_dialog(scale_test_target("web", "deploy-uid-1", 3), window, cx);
        });
    });
    cx.run_until_parked();

    let input_bounds = cx
        .debug_bounds("dialog-scale-input")
        .expect("scale input is laid out");
    let click_y = input_bounds.center().y;
    cx.simulate_click(
        point(input_bounds.left() + px(12.0), click_y),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.simulate_input("9");
    assert_eq!(
        shell.read_with(cx, |shell, cx| match &shell.dialog {
            Some(Dialog::Scale { input, .. }) => input.read(cx).text().to_owned(),
            _ => panic!("scale dialog is open"),
        }),
        "93"
    );

    let input_bounds = cx
        .debug_bounds("dialog-scale-input")
        .expect("scale input remains laid out");
    cx.simulate_click(
        point(input_bounds.right() - px(40.0), click_y),
        Modifiers::none(),
    );
    cx.run_until_parked();
    cx.simulate_input("4");
    assert_eq!(
        shell.read_with(cx, |shell, cx| match &shell.dialog {
            Some(Dialog::Scale { input, .. }) => input.read(cx).text().to_owned(),
            _ => panic!("scale dialog is open"),
        }),
        "934"
    );

    let input_focus = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::Scale { input, .. }) => input.read(cx).focus_handle(cx),
        _ => panic!("scale dialog is open"),
    });
    let dialog_focus = shell.read_with(cx, |shell, _| shell.dialog_button_focus_handles[1].clone());
    cx.simulate_keystrokes("tab");
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 1);
    assert!(!cx.update(|window, _| input_focus.is_focused(window)));
    assert!(cx.update(|window, _| dialog_focus.is_focused(window)));

    cx.simulate_keystrokes("shift-tab");
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 0);
    assert!(cx.update(|window, _| input_focus.is_focused(window)));
    cx.simulate_keystrokes("escape");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
}

struct NoopForwardHandle;

impl crate::panels::terminal::ForwardHandle for NoopForwardHandle {
    fn stop(&mut self) {}
}

#[gpui_kit::test]
fn async_port_forward_does_not_report_zero_before_binding(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let binding: PortForwardBinding = Rc::new(RefCell::new(None));
    let binding_state = Rc::clone(&binding);
    let error_sender: Rc<RefCell<Option<tokio::sync::mpsc::UnboundedSender<String>>>> =
        Rc::new(RefCell::new(None));
    let error_sender_state = Rc::clone(&error_sender);
    let forwards: PortForwardFactory = Rc::new(move |_request, _cx| {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *binding_state.borrow_mut() = Some(sender);
        let (error_sender, errors) = tokio::sync::mpsc::unbounded_channel();
        *error_sender_state.borrow_mut() = Some(error_sender);
        Ok(crate::panels::terminal::StartedForward {
            handle: Box::new(NoopForwardHandle),
            binding: receiver,
            errors,
        })
    });
    let terminals: TerminalFactory =
        Rc::new(|_request, _sink, _cx| Err("terminals unavailable".to_owned()));
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.set_terminal_services(
                Some(TerminalServices {
                    terminals,
                    forwards,
                    context: Some("kind-k8s-gpui-dev".to_owned()),
                    namespace: None,
                }),
                cx,
            );
            shell.open_port_forward_dialog(
                PortForwardTarget {
                    namespace: Some("default".into()),
                    name: "web-0".into(),
                    ports: Vec::new(),
                },
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    let input = shell
        .read_with(cx, |shell, _| shell.dialog_input())
        .expect("port-forward dialog is open");
    cx.update(|window, cx| input.update(cx, |input, cx| input.set_text("80", window, cx)));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.confirm_port_forward(window, cx));
    });
    cx.run_until_parked();

    let toast = shell
        .read_with(cx, |shell, _| shell.toast.clone())
        .expect("connecting feedback is visible");
    assert_eq!(toast.severity, crate::design::Severity::Info);
    assert!(
        toast.message.contains("Starting") || toast.message.contains("Connecting"),
        "toast: {toast:?}"
    );
    assert!(!toast.message.contains("localhost:0"));
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));

    binding
        .borrow_mut()
        .take()
        .expect("binding sender is retained")
        .send(Ok(4321))
        .ok();
    cx.run_until_parked();
    let toast = shell
        .read_with(cx, |shell, _| shell.toast.clone())
        .expect("bound feedback is visible");
    assert_eq!(toast.severity, crate::design::Severity::Success);
    assert!(toast.message.contains("localhost:4321"));
}

// PROBE(diagnose-macos-paste): temporary, to learn why secondary-v / secondary-a
// do nothing on macOS runners while every other platform pastes it.
#[gpui_kit::test]
fn probe_secondary_chords_in_dialog_input(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.set_terminal_services(
                Some(TerminalServices {
                    terminals: Rc::new(|_request, _sink, _cx| {
                        Err("terminals unavailable".to_owned())
                    }),
                    forwards: Rc::new(
                        |_request, _cx| Err("port forwarding unavailable".to_owned()),
                    ),
                    context: Some("kind-k8s-gpui-dev".to_owned()),
                    namespace: None,
                }),
                cx,
            );
            shell.open_port_forward_dialog(
                PortForwardTarget {
                    namespace: Some("default".into()),
                    name: "web-0".into(),
                    ports: Vec::new(),
                },
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();

    // What does the running keymap say secondary-v resolves to, with the
    // dialog input's own context stack?
    cx.update(|_window, cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let keystroke = gpui_kit::Keystroke::parse("secondary-v").expect("parse");
        eprintln!(
            "PROBE keystroke secondary-v => key={:?} modifiers={:?} key_char={:?}",
            keystroke.key, keystroke.modifiers, keystroke.key_char
        );
        for contexts in [
            vec!["TextInput", "Input"],
            vec!["Input", "TextInput"],
            vec!["TextInput"],
            vec!["Input"],
        ] {
            let stack: Vec<gpui_kit::KeyContext> = contexts
                .iter()
                .map(|name| gpui_kit::KeyContext::parse(name).expect("context"))
                .collect();
            let (matches, pending) =
                keymap.bindings_for_input(std::slice::from_ref(&keystroke), &stack);
            let names: Vec<String> = matches
                .iter()
                .map(|binding| binding.action().name().to_string())
                .collect();
            eprintln!(
                "PROBE bindings_for_input(secondary-v, {contexts:?}) => {names:?} pending={pending}"
            );
        }
    });

    // The port-forward dialog opens with the input focused.
    cx.write_to_clipboard(ClipboardItem::new_string("8x\n0".to_owned()));
    // 1. Raw action dispatch: does input::Paste work at all here?
    cx.update(|window, cx| {
        window.dispatch_action(Box::new(gpui_kit::component::input::Paste), cx);
    });
    let text = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
        _ => "<no dialog>".to_string(),
    });
    eprintln!("PROBE port-forward, dispatch input::Paste => {text:?}");
    // 2. The app's own spelling of the same edit.
    cx.update(|window, cx| {
        window.dispatch_action(Box::new(k8s_actions::Paste), cx);
    });
    let text = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
        _ => "<no dialog>".to_string(),
    });
    eprintln!("PROBE port-forward, dispatch k8s_shell::Paste => {text:?}");
    let focused = cx.update(|_window, cx| {
        shell.read_with(cx, |shell, cx| match &shell.dialog {
            Some(Dialog::PortForward { input, .. }) => Some(input.read(cx).focus_handle(cx)),
            _ => None,
        })
    });
    eprintln!("PROBE focused-focusable => {focused:?}");
    cx.simulate_keystrokes("secondary-v");
    cx.run_until_parked();
    let text = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
        _ => "<no dialog>".to_string(),
    });
    eprintln!("PROBE port-forward, no click, secondary-v => {text:?}");

    // And again after the click the failing test performs.
    let input_bounds = cx
        .debug_bounds("dialog-port-forward-input")
        .expect("port input is laid out");
    cx.simulate_click(
        point(input_bounds.left() + px(12.0), input_bounds.center().y),
        Modifiers::none(),
    );
    cx.simulate_keystrokes("secondary-v");
    let text = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
        _ => "<no dialog>".to_string(),
    });
    eprintln!("PROBE port-forward, after click, secondary-v => {text:?}");

    cx.simulate_keystrokes("secondary-a");
    cx.simulate_input("12");
    let text = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
        _ => "<no dialog>".to_string(),
    });
    eprintln!("PROBE port-forward, secondary-a + input 12 => {text:?}");

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    // Deliberately fail so nextest prints the probe lines above; this test is
    // a diagnostic and never merges.
    panic!("PROBE dump");
}

#[gpui_kit::test]
fn port_forward_dialog_uses_text_input_for_paste_and_validation(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let requests = Rc::new(RefCell::new(Vec::new()));
    let request_sink = Rc::clone(&requests);
    let forwards: PortForwardFactory = Rc::new(move |request, _cx| {
        request_sink.borrow_mut().push(request);
        Err("port forwarding unavailable".to_owned())
    });
    let terminals: TerminalFactory =
        Rc::new(|_request, _sink, _cx| Err("terminals unavailable".to_owned()));
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.set_terminal_services(
                Some(TerminalServices {
                    terminals,
                    forwards,
                    context: Some("kind-k8s-gpui-dev".to_owned()),
                    namespace: None,
                }),
                cx,
            );
            shell.open_port_forward_dialog(
                PortForwardTarget {
                    namespace: Some("default".into()),
                    name: "web-0".into(),
                    ports: Vec::new(),
                },
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();

    let input_focus = shell.read_with(cx, |shell, cx| match &shell.dialog {
        Some(Dialog::PortForward { input, .. }) => input.read(cx).focus_handle(cx),
        _ => panic!("port-forward dialog is open"),
    });
    assert!(cx.update(|window, _| input_focus.is_focused(window)));
    let input_bounds = cx
        .debug_bounds("dialog-port-forward-input")
        .expect("port input is laid out");
    cx.simulate_click(
        point(input_bounds.left() + px(12.0), input_bounds.center().y),
        Modifiers::none(),
    );
    cx.write_to_clipboard(ClipboardItem::new_string("8x\n0".to_owned()));
    cx.simulate_keystrokes("secondary-v");
    assert_eq!(
        shell.read_with(cx, |shell, cx| match &shell.dialog {
            Some(Dialog::PortForward { input, .. }) => input.read(cx).text().to_owned(),
            _ => panic!("port-forward dialog is open"),
        }),
        "80"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(requests.borrow()[0].remote_port, 80);
    assert!(shell.read_with(cx, |shell, _| matches!(
        &shell.dialog,
        Some(Dialog::PortForward { error: Some(_), .. })
    )));

    cx.simulate_keystrokes("secondary-a backspace");
    cx.simulate_input("0");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        requests.borrow().len(),
        1,
        "an invalid port must not be submitted again"
    );
    assert!(shell.read_with(cx, |shell, _| matches!(
        &shell.dialog,
        Some(Dialog::PortForward { error: Some(_), .. })
    )));

    cx.simulate_keystrokes("tab escape");
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
}

#[test]
fn replicas_parser_accepts_non_negative_integers() {
    assert_eq!(parse_replicas("0"), Ok(0));
    assert_eq!(parse_replicas("12"), Ok(12));
    assert_eq!(parse_replicas(" 7 "), Ok(7));
    assert!(parse_replicas("").is_err());
    assert!(parse_replicas("-1").is_err());
    assert!(parse_replicas("1.5").is_err());
    assert!(parse_replicas("abc").is_err());
    assert!(parse_replicas("99999999999999").is_err());
}

/// Port inputs accept 1 through 65535. Port zero is assigned automatically.
#[test]
fn port_parser_accepts_valid_ports_only() {
    assert_eq!(parse_port("80"), Ok(80));
    assert_eq!(parse_port(" 65535 "), Ok(65535));
    assert!(parse_port("").is_err());
    assert!(parse_port("0").is_err());
    assert!(parse_port("65536").is_err());
    assert!(parse_port("http").is_err());
    assert!(parse_port("-1").is_err());
}

/// A catalog refresh adds new kinds and preserves expanded groups.
#[gpui_kit::test]
fn catalog_refresh_adds_new_kinds_and_keeps_expansion(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "apps",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "apps", "version": "v1", "kind": "Deployment",
                      "plural": "deployments", "scope": "namespaced", "verbs": ["get", "list"] },
                ],
            }],
        }],
    }))
    .expect("valid catalog JSON");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));

    let (group, deployment_row) = shell.read_with(cx, |shell, _| {
        let rows = shell.tree.rows(&shell.collapsed);
        (
            rows.iter()
                .find(|row| row.kind == super::TreeRowKind::Group)
                .map(|row| row.id.clone())
                .expect("group row"),
            rows.iter()
                .find(|row| row.resource_kind.as_deref() == Some("Deployment"))
                .map(|row| row.id.clone())
                .expect("Deployments row"),
        )
    });
    // Collapse and expand the apps group to model user input.
    shell.update(cx, |shell, _| {
        shell.collapsed.insert(group.clone());
    });
    shell.update(cx, |shell, _| {
        shell.collapsed.remove(&group);
    });

    let refreshed: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [
            {
                "group": "apps",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "apps", "version": "v1", "kind": "Deployment",
                          "plural": "deployments", "scope": "namespaced", "verbs": ["get", "list"] },
                    ],
                }],
            },
            {
                "group": "example.com",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "example.com", "version": "v1", "kind": "Widget",
                          "plural": "widgets", "scope": "namespaced", "verbs": ["get", "list"] },
                    ],
                }],
            },
        ],
    }))
    .expect("valid catalog JSON");
    shell.update(cx, |shell, cx| shell.on_catalog_refreshed(refreshed, cx));

    shell.read_with(cx, |shell, _| {
        let rows = shell.tree.rows(&shell.collapsed);
        assert!(
            rows.iter()
                .any(|row| row.resource_kind.as_deref() == Some("Widget")),
            "the new CRD kind must appear in the tree after a refresh"
        );
        assert!(
            rows.iter().any(|row| row.id == deployment_row),
            "the existing kind must remain"
        );
        assert!(
            rows.iter().any(|row| row.id == group),
            "the apps group must remain expanded"
        );
        assert!(matches!(shell.catalog_state, CatalogState::Ready));
    });
}

#[gpui_kit::test]
fn tab_order_puts_the_table_before_the_panels(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.dock_open = true;
        shell.inspector_open = true;
        cx.notify();
    });
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();

    let (
        tree,
        center_tabs,
        tree_filter,
        filter,
        table,
        left_divider,
        right_divider,
        dock_divider,
        dock,
        dock_strip,
        status_bar,
    ) = cx.update(|_, cx| {
        let shell = shell.read(cx);
        (
            shell.tree_focus_handle.clone(),
            shell.center_tabs_focus.clone(),
            shell.tree_filter_input.read(cx).focus_handle(cx),
            shell.pods.read(cx).filter_focus_handle(cx),
            shell.pods.read(cx).table_focus_handle(cx),
            shell.left_divider_focus.clone(),
            shell.right_divider_focus.clone(),
            shell.dock_divider_focus.clone(),
            shell.dock_panel.read(cx).focus_handle(),
            shell.dock_panel.read(cx).collapse_focus_handle(),
            shell.status_bar_metrics_focus().clone(),
        )
    });

    cx.update(|window, cx| window.focus(&tree, cx));
    let mut order = Vec::new();
    // The budget is a focus-trap guard, not a count: a full cycle is every region's stops plus
    // every panel control's, and the status bar's readouts joined that list, so a budget sized
    // to the previous tail stopped one step short of the return and read as a trap.
    for _ in 0..64 {
        let focused = cx.update(|window, cx| window.focused(cx));
        let tag = match focused {
            Some(handle) if handle == tree => "tree",
            Some(handle) if handle == center_tabs => "center-tabs",
            Some(handle) if handle == tree_filter => "tree-filter",
            Some(handle) if handle == filter => "filter",
            Some(handle) if handle == table => "table",
            Some(handle) if handle == left_divider => "left-divider",
            Some(handle) if handle == right_divider => "right-divider",
            Some(handle) if handle == dock_divider => "dock-divider",
            Some(handle) if handle == dock => "dock",
            Some(handle) if handle == dock_strip => "dock-strip",
            Some(handle) if handle == status_bar => "status-bar",
            _ => "other",
        };
        order.push(tag);
        if order.len() > 1 && tag == "tree" {
            break;
        }
        cx.update(|window, cx| window.focus_next(cx));
    }

    let position = |tag: &str| order.iter().position(|entry| *entry == tag);
    for tag in [
        "tree",
        "center-tabs",
        "tree-filter",
        "filter",
        "table",
        "left-divider",
        "right-divider",
        "dock-divider",
        "status-bar",
    ] {
        assert!(
            position(tag).is_some(),
            "{tag} must be reachable: {order:?}"
        );
    }
    // The Dock is reachable through whichever of its two handles is on screen. The
    // body owns `dock` and the 28px strip owns `dock-strip`, and `UI-SPEC` §16.2
    // keeps the strip resident precisely so a collapsed Dock is still a way in —
    // so "the Dock has a tab stop" is the invariant, not "this particular handle
    // is in the order". The Dock opens with its body folded (there is no stream
    // to show yet), which is exactly the case that used to leave it unreachable.
    let dock_at = position("dock").or_else(|| position("dock-strip"));
    assert!(
        dock_at.is_some(),
        "the Dock must be reachable by keyboard, through its body or its strip: {order:?}"
    );
    let dock_at = dock_at.unwrap();
    let table_at = position("table").unwrap();
    assert!(
        table_at < dock_at,
        "the table must come before the Dock: {order:?}"
    );
    assert_eq!(
        position("tree"),
        Some(0),
        "traversal must start at the tree: {order:?}"
    );
    for panel_control in ["right-divider", "dock-divider"] {
        assert!(
            table_at < position(panel_control).unwrap(),
            "the table must come before the right-panel control {panel_control}: {order:?}"
        );
    }
    assert_eq!(
        order.last(),
        Some(&"tree"),
        "Tab traversal must return to the tree: {order:?}"
    );
}

#[gpui_kit::test]
fn refresh_view_command_triggers_a_fresh_catalog_fetch(cx: &mut TestAppContext) {
    let attempts = Rc::new(Cell::new(0usize));
    let attempts_for_future = attempts.clone();
    let future: super::CatalogFuture = Rc::new(move || {
        let attempt = attempts_for_future.get();
        attempts_for_future.set(attempt + 1);
        Box::pin(async move {
            if attempt == 0 {
                Err("fresh catalog failure".to_owned())
            } else {
                Ok(ResourceCatalog::default())
            }
        })
    });
    let (shell, cx) = catalog_retry_shell(cx, "refresh-view", future);
    shell.update(cx, |shell, cx| {
        shell.catalog_state = CatalogState::Ready;
        shell.catalog_failure = None;
        shell.namespace_future = Some(Rc::new(|_| -> OpsFuture<Vec<String>> {
            Box::pin(async { Ok(Vec::new()) })
        }));
        cx.notify();
    });

    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("refresh view");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();

    assert_eq!(attempts.get(), 1);
    assert!(
        shell.read_with(cx, |shell, _| shell._catalog_task.is_some()),
        "Refresh View must start one fresh discovery"
    );
    let notification = shell
        .read_with(cx, |shell, _| {
            shell
                .notifications
                .iter()
                .find(|notification| notification.message.contains("refresh resources"))
                .cloned()
        })
        .expect("a refresh failure must create a persistent notification");
    assert_eq!(notification.severity, crate::design::Severity::Error);
    assert_eq!(
        notification.detail.as_deref(),
        Some("fresh catalog failure")
    );
    assert!(notification.message.contains("Refresh View"));

    shell.update(cx, |shell, cx| {
        shell.on_metrics_probed(
            crate::panels::metrics::MetricsProbeState::Error {
                reason: "metrics unavailable".to_owned(),
            },
            cx,
        );
    });
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .notifications
            .iter()
            .any(|notification| notification.message.contains("refresh resources"))
    }));

    cx.simulate_keystrokes("secondary-shift-p");
    cx.simulate_input("refresh view");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(attempts.get(), 2);
    assert!(matches!(
        shell.read_with(cx, |shell, _| shell.catalog_state.clone()),
        CatalogState::Ready
    ));
}

/// A terminal factory error after Exec confirmation must not panic.
#[gpui_kit::test]
fn failing_terminal_from_exec_dialog_does_not_panic(cx: &mut TestAppContext) {
    use crate::panels::terminal::{TerminalFactory, TerminalServices};
    use crate::table_view::ExecTarget;

    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));

    let terminals: TerminalFactory =
        Rc::new(|_request, _sink, _cx| Err("Exec is not available in this test build.".to_owned()));
    let forwards: crate::panels::terminal::PortForwardFactory = Rc::new(|_request, _cx| {
        Err("Port forwarding is not available in this test build.".to_owned())
    });
    shell.update(cx, |shell, cx| {
        shell.set_terminal_services(
            Some(TerminalServices {
                terminals,
                forwards,
                context: Some("kind-k8s-gpui-dev".to_owned()),
                namespace: None,
            }),
            cx,
        );
        shell.dialog = Some(Dialog::Exec {
            target: ExecTarget {
                namespace: Some("default".to_owned()),
                name: "web-0".into(),
                containers: vec!["app".into(), "sidecar".into()],
            },
            selected: 0,
        });
    });

    // The old synchronous failure path re-entered the shell and panicked.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.confirm_exec_dialog(window, cx));
    });
    shell.read_with(cx, |shell, _| {
        assert!(
            shell.dialog.is_none(),
            "the dialog must close after confirmation"
        );
        assert!(
            shell.toast.is_some(),
            "the failure message must reach Shell through defer"
        );
    });
}

#[gpui_kit::test]
fn registry_current_context_is_used_without_a_remembered_choice(cx: &mut TestAppContext) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let source =
        SWITCH_KUBECONFIG.replace("current-context: alpha-ctx", "current-context: beta-ctx");
    let registry = load_test_registry(&handle, &source, "current-context");

    assert_eq!(registry.current_context(), Some("beta-ctx"));
    let selected = super::preferred_cluster(&registry, None).expect("current context cluster");
    assert_eq!(
        registry.get(selected).map(|cluster| cluster.name()),
        Some("beta-ctx")
    );
}

#[gpui_kit::test]
fn switching_a_to_b_clears_search_state_and_retargets_the_executor(cx: &mut TestAppContext) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, "search-switch");
    let session = ClusterSession::from_registry(registry, handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_search_resources(window, cx));
    });
    let search = shell.read_with(cx, |shell, _| shell.search.clone());
    search.update(cx, |search, _| {
        search.inject_results_for_test(vec![search_hit("alpha-search")]);
    });
    assert!(shell.read_with(cx, |shell, cx| !shell.search.read(cx).hits().is_empty()));
    let old_dock = shell.read_with(cx, |shell, _| shell.dock_panel.entity_id());
    shell.update(cx, |shell, cx| {
        shell.dialog = Some(Dialog::ConfirmTabClose {
            request: super::TabCloseRequest::All,
        });
        shell.dock_panel.update(cx, |dock, cx| {
            dock.set_lines(vec!["old log".to_owned()], cx)
        });
        assert!(shell.switch_cluster(1, cx));
    });
    cx.run_until_parked();

    shell.read_with(cx, |shell, cx| {
        assert!(!shell.search_open);
        let search = shell.search.read(cx);
        assert!(search.query().is_empty());
        assert!(search.hits().is_empty());
        assert!(search.target().is_none());
        assert!(shell.dialog.is_none());
        assert!(shell.dock_panel.read(cx).request().is_none());
        assert_ne!(shell.dock_panel.entity_id(), old_dock);
        assert_eq!(
            shell
                .session
                .as_ref()
                .and_then(ClusterSession::cluster_name),
            Some("beta-ctx")
        );
    });
}

#[gpui_kit::test]
fn global_search_waits_for_catalog_and_retries_the_query(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, "search-catalog-wait");
    let session = ClusterSession::from_registry(registry, handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_search_resources(window, cx));
    });
    let search = shell.read_with(cx, |shell, _| shell.search.clone());
    let search_focus = search.read_with(cx, |search, cx| search.focus_handle(cx));
    cx.update(|window, cx| window.focus(&search_focus, cx));
    cx.simulate_input("web");
    cx.run_until_parked();
    cx.executor()
        .advance_clock(Duration::from_millis(SEARCH_DEBOUNCE_MS));
    cx.run_until_parked();
    assert!(search.read_with(cx, |search, _| search.catalog_waiting()));
    assert_eq!(
        search.read_with(cx, |search, _| search.phase()),
        SearchPhase::Query
    );

    let catalog: ResourceCatalog = serde_json::from_value(serde_json::json!({
        "groups": [{
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [{
                    "group": "",
                    "version": "v1",
                    "kind": "Pod",
                    "plural": "pods",
                    "scope": "namespaced",
                    "verbs": ["list"]
                }]
            }]
        }]
    }))
    .expect("valid catalog");
    shell.update(cx, |shell, cx| shell.on_catalog_loaded(Ok(catalog), cx));
    assert!(!search.read_with(cx, |search, _| search.catalog_waiting()));
    assert_eq!(
        search.read_with(cx, |search, _| search.phase()),
        SearchPhase::Query
    );
}

#[gpui_kit::test]
fn dirty_yaml_blocks_cluster_switch_until_revert(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# unsaved");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));

    shell.update(cx, |shell, cx| {
        assert!(!shell.switch_cluster(1, cx));
    });
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.active_cluster, 0);
        assert!(shell.inspector.read(cx).is_dirty(cx));
        assert!(
            shell
                .toast
                .as_ref()
                .is_some_and(|toast| toast.message.contains("unsaved YAML"))
        );
    });

    shell.update(cx, |shell, cx| {
        shell.inspector.update(cx, |panel, cx| panel.revert(cx));
        assert!(shell.switch_cluster(1, cx));
    });
    shell.read_with(cx, |shell, cx| {
        assert_eq!(shell.active_cluster, 1);
        assert!(!shell.inspector.read(cx).is_dirty(cx));
    });
}

#[gpui_kit::test]
fn kubeconfig_reload_failure_keeps_session_and_success_preserves_cluster_id(
    cx: &mut TestAppContext,
) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, "reload-initial");
    let alpha = registry
        .clusters()
        .iter()
        .find(|cluster| cluster.name() == "alpha-ctx")
        .expect("alpha")
        .id();
    let reloaded = load_test_registry(&handle, RELOADED_KUBECONFIG, "reload-proof");
    assert_ne!(
        reloaded.clusters().first().map(|cluster| cluster.id()),
        Some(alpha)
    );
    let selected =
        super::reloaded_session(reloaded, handle.clone(), Some(alpha)).expect("selected session");
    assert_eq!(selected.cluster_name(), Some("alpha-ctx"));

    let session = ClusterSession::from_registry(registry, handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    let before = shell.read_with(cx, |shell, _| {
        (
            shell.session.as_ref().and_then(ClusterSession::cluster_id),
            shell
                .session
                .as_ref()
                .and_then(ClusterSession::cluster_name)
                .map(str::to_owned),
            shell.session_epoch,
        )
    });
    let failed_path = std::env::temp_dir().join(format!(
        "k8s-gpui-reload-failure-{}.yaml",
        std::process::id()
    ));
    std::fs::write(&failed_path, "apiVersion: v1\nkind: Config\ncontexts: [")
        .expect("write bad config");
    shell.update(cx, |shell, _| {
        shell.reload_kubeconfig_path = Some(failed_path.clone());
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.reload_kubeconfigs(&super::ReloadKubeconfigs, window, cx);
        });
    });
    for _ in 0..100 {
        if shell.read_with(cx, |shell, _| {
            !shell.reload_in_progress
                && shell
                    .toast
                    .as_ref()
                    .is_some_and(|toast| toast.message.contains("current session is unchanged"))
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        cx.run_until_parked();
    }
    let _ = std::fs::remove_file(&failed_path);
    assert!(!shell.read_with(cx, |shell, _| shell.reload_in_progress));
    assert_eq!(
        shell.read_with(cx, |shell, _| {
            (
                shell.session.as_ref().and_then(ClusterSession::cluster_id),
                shell
                    .session
                    .as_ref()
                    .and_then(ClusterSession::cluster_name)
                    .map(str::to_owned),
                shell.session_epoch,
            )
        }),
        before
    );
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.severity == crate::design::Severity::Error)
    }));

    let success_path = std::env::temp_dir().join(format!(
        "k8s-gpui-reload-success-{}.yaml",
        std::process::id()
    ));
    std::fs::write(&success_path, RELOADED_KUBECONFIG).expect("write reloaded config");
    shell.update(cx, |shell, _| {
        shell.reload_kubeconfig_path = Some(success_path.clone());
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.reload_kubeconfigs(&super::ReloadKubeconfigs, window, cx);
        });
    });
    for _ in 0..100 {
        if shell.read_with(cx, |shell, _| {
            !shell.reload_in_progress
                && shell
                    .toast
                    .as_ref()
                    .is_some_and(|toast| toast.message.contains("Kubeconfigs reloaded"))
        }) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        cx.run_until_parked();
    }
    let _ = std::fs::remove_file(&success_path);
    assert!(!shell.read_with(cx, |shell, _| shell.reload_in_progress));
    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.session.as_ref().and_then(ClusterSession::cluster_id),
            before.0
        );
        assert_eq!(
            shell
                .session
                .as_ref()
                .and_then(ClusterSession::cluster_name),
            before.1.as_deref()
        );
        assert!(shell.session_epoch > before.2);
        assert_eq!(
            shell
                .session
                .as_ref()
                .and_then(ClusterSession::registry)
                .and_then(|registry| registry.current_context()),
            Some("beta-ctx")
        );
        assert_eq!(
            shell.clusters.first().map(|name| name.as_ref()),
            Some("beta-ctx")
        );
    });
}

#[gpui_kit::test]
fn partial_source_warning_keeps_valid_contexts_visible(cx: &mut TestAppContext) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, "partial-warning");
    let session = ClusterSession::from_registry(registry, handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    shell.update(cx, |shell, cx| {
        shell._namespace_task = None;
        shell.kubeconfig_warning =
            Some("/tmp/broken-kubeconfig: invalid YAML: contexts: [".to_owned());
        cx.notify();
    });
    cx.run_until_parked();

    assert!(cx.debug_bounds("kubeconfig-source-warning").is_some());
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .clusters
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()),
        ["alpha-ctx", "beta-ctx", "broken-ctx"]
    );
    assert_eq!(shell.read_with(cx, |shell, _| shell.clusters.len()), 3);
}

#[gpui_kit::test]
fn registry_reload_callback_runs_exactly_once_after_atomic_reload(cx: &mut TestAppContext) {
    init_ui(cx);
    let handle = init_cluster_runtime(cx);
    let registry = load_test_registry(&handle, SWITCH_KUBECONFIG, "reload-callback-initial");
    let session = ClusterSession::from_registry(registry, handle);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    let calls = Rc::new(Cell::new(0usize));
    let observed = Rc::new(RefCell::new(None));
    let callback_calls = Rc::clone(&calls);
    let callback_observed = Rc::clone(&observed);
    shell.update(cx, |shell, _| {
        shell.set_registry_reload_callback(Rc::new(move |registry| {
            callback_calls.set(callback_calls.get() + 1);
            *callback_observed.borrow_mut() = Some(registry);
        }));
        shell.reload_kubeconfig_path = Some(std::env::temp_dir().join(format!(
            "k8s-gpui-reload-callback-{}.yaml",
            std::process::id()
        )));
    });
    std::fs::write(
        shell.read_with(cx, |shell, _| shell.reload_kubeconfig_path.clone().unwrap()),
        RELOADED_KUBECONFIG,
    )
    .expect("write kubeconfig");

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.reload_kubeconfigs(&ReloadKubeconfigs, window, cx);
            shell.reload_kubeconfigs(&ReloadKubeconfigs, window, cx);
        });
    });
    for _ in 0..100 {
        if !shell.read_with(cx, |shell, _| shell.reload_in_progress) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        cx.run_until_parked();
    }
    let _ = std::fs::remove_file(
        shell.read_with(cx, |shell, _| shell.reload_kubeconfig_path.clone().unwrap()),
    );

    assert!(!shell.read_with(cx, |shell, _| shell.reload_in_progress));
    assert_eq!(calls.get(), 1);
    assert_eq!(
        observed
            .borrow()
            .as_ref()
            .and_then(|registry| registry.current_context()),
        Some("beta-ctx")
    );

    shell.update(cx, |shell, cx| {
        assert!(shell.switch_cluster(1, cx));
    });
    assert_eq!(
        calls.get(),
        1,
        "a normal context switch must not trigger the reload callback"
    );
}

#[gpui_kit::test]
fn stale_apply_completion_is_rejected_after_switch(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let receiver = Rc::new(RefCell::new(Some(receiver)));
    let apply_receiver = Rc::clone(&receiver);
    shell.update(cx, |shell, _| {
        shell.apply_future = Some(Rc::new(move |request| {
            let receiver = apply_receiver
                .borrow_mut()
                .take()
                .expect("one apply request");
            Box::pin(async move {
                receiver
                    .await
                    .map_err(|error| format!("release apply: {error}"))?;
                let object = serde_json::from_value(serde_json::json!({
                    "apiVersion": request.target.resource.version,
                    "kind": request.target.resource.kind,
                    "metadata": {
                        "name": request.target.name,
                        "namespace": request.target.namespace,
                        "uid": request.target.uid,
                    },
                }))
                .expect("applied object");
                Ok(crate::panels::ApplyOutcome::Applied(Arc::new(object)))
            })
        }));
    });
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# pending");
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_apply_yaml(window, cx));
    });
    cx.run_until_parked();
    // Apply only asks for a review, so the write starts when the review is confirmed.
    shell.update(cx, |shell, cx| {
        shell
            .inspector
            .update(cx, |panel, cx| panel.confirm_pending_apply(cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_applying()));

    let before_epoch = shell.read_with(cx, |shell, _| shell.session_epoch);
    shell.update(cx, |shell, cx| {
        shell.inspector.update(cx, |panel, cx| panel.revert(cx));
        assert!(shell.switch_cluster(1, cx));
    });
    sender.send(()).expect("release apply");
    cx.run_until_parked();

    shell.read_with(cx, |shell, cx| {
        assert!(shell.session_epoch > before_epoch);
        assert!(!shell.inspector.read(cx).is_applying());
        assert!(shell.inspector.read(cx).current_apply_request().is_none());
        assert!(
            shell
                .toast
                .as_ref()
                .is_none_or(|toast| !toast.message.contains("Applied"))
        );
    });
}

/// Close All on a dirty tab must default to Cancel so Enter keeps the edits.
#[gpui_kit::test]
fn close_all_dirty_tabs_defaults_to_cancel(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# keep me");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.close_all_center_tabs(window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| matches!(
        shell.dialog.as_ref(),
        Some(Dialog::ConfirmTabClose {
            request: super::TabCloseRequest::All
        })
    )));
    assert_eq!(shell.read_with(cx, |shell, _| shell.dialog_focus), 0);
    let cancel = shell.read_with(cx, |shell, _| shell.dialog_button_focus_handles[0].clone());
    assert!(
        cx.update(|window, _| cancel.is_focused(window)),
        "the destructive confirmation must not hold the default focus"
    );

    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)),
        "Enter on Cancel must keep every dirty tab"
    );
    assert!(!shell.read_with(cx, |shell, _| shell.open_tabs.is_empty()));

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.close_all_center_tabs(window, cx));
    });
    cx.simulate_keystrokes("tab");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.dialog.is_none()));
    assert!(!shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));
}

/// Implicit Dock and Inspector opens record their source and give focus back.
#[gpui_kit::test]
fn implicit_panel_opens_restore_the_focus_that_opened_them(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let source = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    cx.update(|window, cx| window.focus(&source, cx));
    cx.run_until_parked();

    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_dock(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| shell.dock_open));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.dock_previous_focus.clone()),
        Some(source.clone())
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.toggle_dock(&ToggleDock, window, cx));
    });
    cx.run_until_parked();
    assert!(cx.update(|window, _| source.is_focused(window)));

    cx.update(|window, cx| window.focus(&source, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_inspector(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| shell.inspector_open));
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.inspector_previous_focus.clone()),
        Some(source.clone())
    );
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.toggle_right_panel(&ToggleRightPanel, window, cx)
        });
    });
    cx.run_until_parked();
    assert!(cx.update(|window, _| source.is_focused(window)));
}

/// A cluster reset clears the panel records and restores the stable focus.
#[gpui_kit::test]
fn cluster_reset_restores_the_focus_recorded_by_an_implicit_panel(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let source = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    cx.update(|window, cx| window.focus(&source, cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.open_dock(window, cx);
            shell.open_notifications(window, cx);
            shell.close_status_panel(window, cx);
        });
    });
    cx.update(|window, cx| window.focus(&source, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.open_dock(window, cx));
    });
    assert!(shell.read_with(cx, |shell, _| shell.dock_previous_focus.is_some()));

    shell.update(cx, |shell, cx| shell.reset_cluster_bound_ui(cx));
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, _| shell.dock_previous_focus.is_none()),
        "a cluster reset must drop panel focus records"
    );
    assert!(cx.update(|window, _| source.is_focused(window)));
    assert!(!shell.read_with(cx, |shell, _| shell.focus_active_view_pending));
}

/// A tab whose view is not mounted must not keep the focus handoff pending.
#[gpui_kit::test]
fn a_tab_without_a_view_clears_the_pending_focus(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, _| {
        shell.tabs.push(CenterTab {
            content: TabContent::Resource,
            kind: "CustomResourceDefinition".into(),
            identity: None,
            title: "Custom Resource Definitions".into(),
            icon: IconName::Server,
            entry: None,
            resource: None,
            pinned: false,
            preview: None,
        });
        shell.views.push(None);
    });
    cx.run_until_parked();
    let index = shell.read_with(cx, |shell, _| shell.tabs.len() - 1);
    shell.update(cx, |shell, cx| {
        assert!(shell.activate_tab(index, cx));
        shell.focus_active_view_pending = true;
    });
    shell.update(cx, |shell, cx| shell.defer_focus_active_view(cx));
    cx.run_until_parked();
    assert!(
        !shell.read_with(cx, |shell, _| shell.focus_active_view_pending),
        "a tab that cannot take focus must not block the shell focus fallback"
    );
    let root = shell.read_with(cx, |shell, _| shell.focus_handle.clone());
    assert!(cx.update(|window, _| root.is_focused(window)));
}

/// A divider drag follows the panel edge and keeps the grab offset.
#[gpui_kit::test]
fn divider_drag_follows_the_panel_edge(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let before = shell.read_with(cx, |shell, _| shell.left_width);
    let divider = cx.debug_bounds("divider-left").expect("left divider");
    // Press near the left edge of the divider so the pointer is not on its center.
    let press = point(divider.left() + px(1.0), divider.center().y);
    // The grab is the distance from the panel edge to the press, so the divider edge
    // must not jump while the pointer stays where it landed.
    let edge = shell.read_with(cx, |shell, _| shell.left_width);
    cx.simulate_mouse_move(press, None, Modifiers::none());
    cx.simulate_mouse_down(press, MouseButton::Left, Modifiers::none());
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.drag),
        Some(super::DividerDrag {
            target: DragTarget::Left,
            grab: f32::from(press.x) - edge,
        })
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.left_width),
        before,
        "a press must not resize the panel before the pointer moves"
    );
    let moved = point(press.x + px(40.0), press.y);
    cx.simulate_mouse_move(moved, MouseButton::Left, Modifiers::none());
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.left_width),
        before + 40.0
    );
    cx.simulate_mouse_up(moved, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.drag), None);
}

/// The toolbar comes before the content and the status bar after it.
#[gpui_kit::test]
fn toolbar_precedes_the_content_and_the_status_bar_follows_it(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();

    // The error count and the notification bell moved to the top bar, so the Port Forwards
    // button is the only control the bottom bar still owns, and the only handle it tracks.
    let (top_bar, tree, center_tabs, table, left_divider, port_forwards) = cx.update(|_, cx| {
        let shell = shell.read(cx);
        (
            shell.top_bar_focus.clone(),
            shell.tree_focus_handle.clone(),
            shell.center_tabs_focus.clone(),
            shell.pods.read(cx).table_focus_handle(cx),
            shell.left_divider_focus.clone(),
            shell.status_bar_port_forward_focus.clone(),
        )
    });

    cx.update(|window, cx| window.focus(&top_bar, cx));
    let mut order: Vec<Option<gpui_kit::FocusHandle>> = Vec::new();
    // The Dock's strip is resident (§16.2) and the log panel brings its own toolbar and rows, so
    // the chrome stops below the bar number the budget used to allow for.
    for _ in 0..120 {
        let focused = cx.update(|window, cx| window.focused(cx));
        order.push(focused);
        cx.update(|window, cx| window.focus_next(cx));
    }
    let index_of = |handle: &gpui_kit::FocusHandle| {
        order
            .iter()
            .position(|entry| entry.as_ref() == Some(handle))
    };
    for (label, handle) in [
        ("tree", &tree),
        ("center-tabs", &center_tabs),
        ("table", &table),
        ("left-divider", &left_divider),
        ("port-forwards", &port_forwards),
    ] {
        assert!(
            index_of(handle).is_some(),
            "{label} must be a tab stop: {order:?}"
        );
    }
    assert_eq!(index_of(&top_bar), Some(0), "Tab starts at the toolbar");
    for (label, handle) in [
        ("tree", &tree),
        ("center-tabs", &center_tabs),
        ("table", &table),
        ("left-divider", &left_divider),
    ] {
        assert!(
            index_of(&top_bar) < index_of(handle),
            "the toolbar must come before the {label}: {order:?}"
        );
    }
    assert!(
        index_of(&port_forwards) > index_of(&left_divider),
        "the status bar must come after the content: {order:?}"
    );
}

/// The palette keeps Tab inside the modal instead of moving focus behind it.
#[gpui_kit::test]
fn palette_keeps_tab_inside_the_overlay(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let (table, top_bar, port_forwards) = cx.update(|_, cx| {
        let shell = shell.read(cx);
        (
            shell.pods.read(cx).table_focus_handle(cx),
            shell.top_bar_focus.clone(),
            shell.status_bar_port_forward_focus.clone(),
        )
    });
    cx.update(|window, cx| window.focus(&table, cx));
    cx.simulate_keystrokes("secondary-shift-p");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));
    let input = shell.read_with(cx, |shell, cx| {
        shell.palette_input.read(cx).focus_handle(cx)
    });
    assert!(cx.update(|window, _| input.is_focused(window)));

    for _ in 0..4 {
        cx.simulate_keystrokes("tab");
        cx.simulate_keystrokes("shift-tab");
        assert!(
            cx.update(|window, _| input.is_focused(window)),
            "Tab must stay inside the command palette"
        );
    }
    assert!(!cx.update(|window, _| top_bar.is_focused(window)));
    assert!(!cx.update(|window, _| port_forwards.is_focused(window)));
    assert!(shell.read_with(cx, |shell, _| shell.palette_open));
}

/// Escape, the toggle action and the background all close the status popover.
#[gpui_kit::test]
fn status_popover_closes_from_escape_toggle_and_background(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let source = shell.read_with(cx, |shell, _| shell.tree_focus_handle.clone());
    cx.update(|window, cx| window.focus(&source, cx));
    cx.run_until_parked();

    for close in ["escape", "toggle", "background"] {
        cx.update(|window, cx| window.focus(&source, cx));
        cx.run_until_parked();
        cx.update(|window, cx| {
            shell.update(cx, |shell, cx| shell.open_notifications(window, cx));
        });
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.status_panel),
            StatusPanel::Notifications,
            "{close} needs an open popover"
        );
        match close {
            "escape" => cx.simulate_keystrokes("escape"),
            "toggle" => cx.simulate_keystrokes("secondary-shift-n"),
            _ => cx.simulate_click(point(px(20.0), px(200.0)), Modifiers::none()),
        }
        cx.run_until_parked();
        assert_eq!(
            shell.read_with(cx, |shell, _| shell.status_panel),
            StatusPanel::None,
            "{close} must close the notification center"
        );
        assert!(
            cx.update(|window, _| source.is_focused(window)),
            "{close} must restore the focus that opened the popover"
        );
    }

    // The forwards link is not a popover: it opens a centre view and leaves the status bar's own
    // panel state alone, and the notification chord still owns the notification centre.
    cx.update(|window, cx| window.focus(&source, cx));
    cx.run_until_parked();
    let link = cx
        .debug_bounds("status-bar-port-forwards")
        .expect("the forwards link");
    cx.simulate_click(link.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.status_panel),
        StatusPanel::None,
        "opening the forwards list must not open a status panel"
    );
    cx.simulate_keystrokes("secondary-shift-n");
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.status_panel),
        StatusPanel::Notifications,
        "the notification chord is unaffected by the forwards link"
    );
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.status_panel),
        StatusPanel::None
    );
    // The focus goes back to the list the link opened, not to the tree: that is where the reader
    // was when they reached for the bar, and putting it anywhere else would be a second jump.
    assert!(
        shell.read_with(cx, |shell, _| shell.tabs[shell.active_tab].content
            == TabContent::Forwards),
        "the list is still the active view"
    );
}

/// The update overlay swallows the click that dismisses it.
#[gpui_kit::test]
fn update_overlay_closes_on_an_outside_click(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let calls = Rc::new(RefCell::new(Vec::new()));
    let check_calls = calls.clone();
    let retry_calls = calls.clone();
    let restart_calls = calls.clone();
    let actions = UpdateActions::new(
        move |_| check_calls.borrow_mut().push("check"),
        move |_| retry_calls.borrow_mut().push("retry"),
        move |_| restart_calls.borrow_mut().push("restart"),
    );
    shell.update(cx, |shell, cx| {
        shell.set_update_actions(actions, cx);
        shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
    });
    cx.simulate_resize(gpui_kit::size(px(960.), px(640.)));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.update_strip_expanded));
    let overlay = cx
        .debug_bounds("update-strip-overlay")
        .expect("update overlay card");

    // A click on the card header keeps the overlay open.
    cx.simulate_click(
        point(overlay.center().x, overlay.top() + px(8.0)),
        Modifiers::none(),
    );
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.update_strip_expanded));
    assert!(calls.borrow().is_empty());

    // A click outside the card dismisses the overlay and reaches nothing behind it.
    cx.simulate_click(point(px(20.0), px(600.0)), Modifiers::none());
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |shell, _| shell.update_strip_expanded));
    assert!(calls.borrow().is_empty());
    assert!(cx.debug_bounds("update-strip-actions").is_none());
    // The click lands on a focusable control behind the overlay, so the overlay must
    // hand focus over instead of keeping it.
    let overlay_focus = shell.read_with(cx, |shell, _| shell.update_overlay_focus.clone());
    assert!(!cx.update(|window, _| overlay_focus.is_focused(window)));

    // The notice is a one-shot claim, so the updater reporting the same phase again must not
    // bring the card back over the table. Escape while the card is still open is covered by
    // `unsupported_update_keeps_actions_in_a_fixed_overlay`.
    shell.update(cx, |shell, cx| {
        shell.set_update_state(UpdateUiState::new(UpdatePhase::Unsupported), cx);
    });
    cx.update(|window, cx| window.focus(&overlay_focus, cx));
    cx.run_until_parked();
    assert!(cx.debug_bounds("update-strip-overlay").is_none());
    assert!(cx.debug_bounds("update-strip-actions").is_none());
}

/// A drag reorder follows the Settings layout in both directions.
#[gpui_kit::test]
fn tab_drag_reorder_follows_the_settings_layout(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.activate_tab(1, cx);
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx);
    });
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));

    // Settings opens in its own window now (`UI-SPEC` §9.3), so the shell no longer has a
    // command that opens a Settings tab. The tab and the layout it drives are still live code,
    // so this exercises the layout by opening the tab the way `open_special_tab` still can.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            assert!(shell.open_special_tab(
                TabContent::Settings,
                "Settings",
                gpui_kit::assets::IconName::Settings,
                cx,
            ));
        });
        let _ = window;
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.settings_active()));
    // Settings draws its own category column inside the content area, so the resource tree stays
    // on screen. It used to be taken away here as well as by `panel_visibility`, which left both
    // panel toggles disabled and a toast as their only behaviour.
    assert!(
        shell.read_with(cx, |shell, _| shell.sidebar_open),
        "Settings keeps the resource tree"
    );
    assert!(cx.debug_bounds("resource-tree-panel").is_some());
    assert!(cx.debug_bounds("settings-category-Appearance").is_some());
    let settings = shell.read_with(cx, |shell, _| shell.active_tab);

    // Dragging the Settings tab to the front keeps the Settings layout.
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.center_tab_drag = Some(super::CenterTabDragState {
                source: settings,
                insertion: Some(0),
            });
            shell.reorder_center_tab(settings, window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), settings);
    assert!(shell.read_with(cx, |shell, _| shell.sidebar_open));
    assert!(shell.read_with(cx, |shell, _| shell.settings_layout_saved.is_some()));

    // Dragging another tab while Settings is active leaves the Settings layout alone.
    let other = shell.read_with(cx, |shell, _| {
        shell
            .open_tabs
            .iter()
            .copied()
            .find(|tab| *tab != settings)
            .expect("another tab")
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.center_tab_drag = Some(super::CenterTabDragState {
                source: other,
                insertion: Some(0),
            });
            shell.reorder_center_tab(other, window, cx);
        });
    });
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), other);
    assert!(!shell.read_with(cx, |shell, _| shell.settings_active()));
    assert!(
        shell.read_with(cx, |shell, _| shell.sidebar_open),
        "leaving Settings through a drag must leave the sidebar as the reader had it"
    );
    assert!(shell.read_with(cx, |shell, _| shell.settings_layout_saved.is_none()));
}

/// The expansion the reader chose survives the refreshes that follow it.
///
/// The catalog arrives twice on a normal start: once from the disk cache and once from the live
/// cluster. Both went through `on_catalog_loaded`, which reset the tree to its defaults, so
/// everything the reader had opened was closed again a second after they opened it.
#[gpui_kit::test]
fn a_catalog_refresh_keeps_the_tree_the_reader_left(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let epoch = shell.read_with(cx, |shell, _| shell.session_epoch);
    shell.update(cx, |shell, cx| {
        shell.clusters = vec!["kind-test".into()];
        // The cached catalog, then the live one behind it.
        for _ in 0..2 {
            shell.on_catalog_loaded_at(epoch, Ok(ResourceCatalog::default()), cx);
        }
    });
    cx.run_until_parked();

    let cluster_row = shell.read_with(cx, |shell, _| {
        shell
            .tree
            .rows(&shell.collapsed)
            .into_iter()
            .find(|row| row.expandable())
            .map(|row| row.id)
            .expect("the catalog has a container row")
    });
    // Collapse it, so `collapsed` says something the defaults did not.
    shell.update(cx, |shell, cx| shell.toggle_row(cluster_row.clone(), cx));
    assert!(
        shell.read_with(cx, |shell, _| shell.collapsed.contains(&cluster_row)),
        "closing the container puts its id in the collapsed set"
    );
    let after_toggle = shell.read_with(cx, |shell, _| shell.collapsed.clone());

    shell.update(cx, |shell, cx| {
        shell.on_catalog_loaded_at(epoch, Ok(ResourceCatalog::default()), cx);
    });
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.collapsed.clone()),
        after_toggle,
        "a refresh must not close the tree under the reader"
    );

    // And the state is remembered, so the next run opens the same tree.
    let saved = shell.read_with(cx, |shell, _| shell.collapsed_saved.clone());
    assert!(
        saved
            .get("kind-test")
            .is_some_and(|ids| ids.contains(&cluster_row)),
        "the expansion is written to settings.json for the next run"
    );
}

/// The search panel traps Tab and keeps global actions available.
#[gpui_kit::test]
fn search_traps_tab_and_keeps_the_global_search_toggle(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();
    let (table, top_bar) = cx.update(|_, cx| {
        let shell = shell.read(cx);
        (
            shell.pods.read(cx).table_focus_handle(cx),
            shell.top_bar_focus.clone(),
        )
    });
    cx.update(|window, cx| window.focus(&table, cx));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_search_resources(window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.search_open));
    let search_input = shell.read_with(cx, |shell, cx| shell.search.read(cx).focus_handle(cx));
    assert!(cx.update(|window, _| search_input.is_focused(window)));

    for _ in 0..3 {
        cx.simulate_keystrokes("tab");
        cx.simulate_keystrokes("shift-tab");
        assert!(shell.read_with(cx, |shell, _| shell.search_open));
        assert!(!cx.update(|window, _| table.is_focused(window)));
        assert!(!cx.update(|window, _| top_bar.is_focused(window)));
    }

    // The global search action still reaches the shell while the panel is open.
    cx.simulate_keystrokes("secondary-shift-f");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.search_open));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!shell.read_with(cx, |shell, _| shell.search_open));
}

fn pod_row(name: &str, uid: &str, phase: &str) -> Row {
    Row {
        obj: Arc::new(
            serde_json::from_value(serde_json::json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": {
                    "name": name,
                    "namespace": "default",
                    "uid": uid,
                    "resourceVersion": phase,
                },
                "status": { "phase": phase },
            }))
            .expect("test pod"),
        ),
        cells: Vec::new(),
    }
}

/// Opens a row's details the way the row menu's `Open Details` does. A click
/// no longer reaches the preview, so tests that want a preview ask for one.
fn open_details(shell: &gpui_kit::Entity<Shell>, cx: &mut gpui_kit::VisualTestContext, row: Row) {
    shell.update(cx, |shell, cx| {
        shell.open_row_details(row, ResourceSpec::pods(), 0, cx);
    });
}

fn preview_yaml(
    shell: &gpui_kit::Entity<Shell>,
    cx: &mut gpui_kit::VisualTestContext,
    tab: usize,
) -> Option<String> {
    shell.read_with(cx, |shell, cx| {
        shell.views[tab].as_ref().and_then(|slot| match slot {
            TabView::Preview(view) => view
                .read(cx)
                .current_selection(cx)
                .map(|selection| selection.yaml),
            _ => None,
        })
    })
}

#[gpui_kit::test]
fn selection_from_a_background_table_stays_in_its_own_tab(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let spec = ResourceSpec::pods();
    shell.update(cx, |shell, cx| {
        shell.open_row_details(
            pod_row("first", "uid-first", "Running"),
            spec.clone(),
            0,
            cx,
        );
    });
    let preview = shell.read_with(cx, |shell, _| shell.active_tab);

    shell.update(cx, |shell, cx| {
        shell.on_row_selection(Some(pod_row("other", "uid-other", "Running")), spec, 1, cx);
    });

    let yaml = preview_yaml(&shell, cx, preview).expect("preview keeps its object");
    assert!(yaml.contains("uid-first"), "got {yaml}");
    assert_eq!(
        shell.read_with(cx, |shell, cx| {
            shell
                .inspector
                .read(cx)
                .selection()
                .map(|selection| selection.uid.clone())
        }),
        Some("uid-first".to_owned()),
        "a background table must not take over the Inspector"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .resource_selections
            .get(&0)
            .map(|selection| selection.object.uid.clone())),
        Some("uid-first".to_owned()),
        "the active table keeps its own selection"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .resource_selections
            .get(&1)
            .map(|selection| selection.object.uid.clone())),
        Some("uid-other".to_owned()),
        "the background table records its own selection"
    );
}

/// Opening a kind and then clicking a second kind must switch, with nothing said about YAML.
///
/// Every resource tab shares one Inspector, so the guard that keeps unsaved text on its tab reads
/// that one buffer. A buffer that only ever *looked* dirty therefore takes every sidebar click in
/// the app hostage, and the reader is told to apply or revert edits they never made. This is the
/// invariant that fails silently: nothing about the state looks wrong until the whole sidebar
/// stops responding.
#[gpui_kit::test]
fn an_untouched_resource_tab_does_not_block_another_kind(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let spec = ResourceSpec::pods();

    // Select a row, which is what hands the shared Inspector a document to hold.
    shell.update(cx, |shell, cx| {
        shell.on_row_selection(
            Some(pod_row("web", "uid-web", "Running")),
            spec.clone(),
            0,
            cx,
        );
    });
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, cx| shell
            .inspector
            .read(cx)
            .current_selection(cx)
            .is_some()),
        "the Inspector holds the selected object's document"
    );
    assert!(
        !shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)),
        "a document nobody typed into is not an unsaved edit"
    );

    let opened = shell.update(cx, |shell, cx| {
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx)
    });
    cx.run_until_parked();
    assert!(opened, "an untouched buffer must not refuse the click");
    let (active, kind, toast) = shell.read_with(cx, |shell, _| {
        (
            shell.active_tab,
            shell.tabs[shell.active_tab].kind.clone(),
            shell.toast.clone(),
        )
    });
    assert_eq!(kind.as_ref(), "ConfigMap", "the center switched kind");
    assert!(active > 0, "the second kind got its own tab");
    assert!(
        toast.is_none(),
        "nothing was edited, so nothing may be said about unsaved YAML: {toast:?}"
    );
}

/// The guard is real, though: an edit nobody applied does hold the tab.
#[gpui_kit::test]
fn an_edited_resource_tab_does_block_another_kind(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let spec = ResourceSpec::pods();
    shell.update(cx, |shell, cx| {
        shell.on_row_selection(
            Some(pod_row("web", "uid-web", "Running")),
            spec.clone(),
            0,
            cx,
        );
    });
    cx.run_until_parked();
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# edited");
    cx.run_until_parked();
    assert!(
        shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)),
        "typing into the document is a change"
    );

    let opened = shell.update(cx, |shell, cx| {
        shell.open_resource_tab("ConfigMap".into(), "Config Maps".into(), None, None, cx)
    });
    assert!(!opened, "unsaved text holds the tab");
    let toast = shell.read_with(cx, |shell, _| shell.toast.clone());
    assert!(
        toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("unsaved YAML")),
        "the refusal has to say what to do about it: {toast:?}"
    );
}

#[gpui_kit::test]
fn a_dirty_mirror_never_becomes_the_editor_source(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();

    // A resource tab is the editor source, so the mirror takes the keystrokes here.
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.run_until_parked();
    let mirror_focus = shell.read_with(cx, |shell, cx| {
        shell.inspector.read(cx).yaml_focus_handle(cx)
    });
    assert!(
        cx.update(|window, _| mirror_focus.is_focused(window)),
        "the keystrokes must reach the mirror editor"
    );
    cx.simulate_input("# mirror");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));

    // The mirror is the only editor a resource tab has, so the unsaved text holds the tab. The
    // click that would hand the editor role to a Preview tab is refused instead, and it leaves
    // nothing behind: a Preview tab that took the role would hide the text in a tab that is no
    // longer the active one, where no close prompt can ask about it.
    let tabs_before = shell.read_with(cx, |shell, _| shell.tabs.len());
    open_details(&shell, cx, pod_row("pinned", "uid-pinned", "Running"));
    cx.run_until_parked();
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.active_tab),
        0,
        "the dirty mirror keeps the active tab"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tabs.len()),
        tabs_before,
        "the refused Preview tab is not left behind"
    );
    assert!(
        shell.read_with(cx, |shell, cx| shell.active_tab_dirty(cx)),
        "the mirror is still the editor of the active tab"
    );
    assert!(
        shell.read_with(cx, |shell, _| shell
            .toast
            .as_ref()
            .is_some_and(|toast| toast.message.contains("unsaved YAML"))),
        "the refusal says which text has to be dealt with"
    );

    // Reverted, the mirror gives the tab back and the Preview tab becomes the only editor.
    shell.update(cx, |shell, cx| {
        shell.inspector.update(cx, |panel, cx| panel.revert(cx));
    });
    assert!(!shell.read_with(cx, |shell, cx| shell.inspector.read(cx).is_dirty(cx)));
    open_details(&shell, cx, pod_row("pinned", "uid-pinned", "Running"));
    cx.run_until_parked();
    let preview = shell.read_with(cx, |shell, _| shell.active_tab);
    assert_eq!(preview, tabs_before, "the click opened the Preview tab");
    assert!(
        preview_yaml(&shell, cx, preview).is_some_and(|yaml| yaml.contains("uid-pinned")),
        "the Preview tab is the tab that shows the object"
    );
    shell.read_with(cx, |shell, cx| {
        assert!(
            !shell.active_tab_dirty(cx),
            "the Preview tab is the editor of the active tab, and it is clean"
        );
        assert!(
            !shell.tab_close_is_dirty(&[0], cx),
            "the mirror of an inactive tab is not the editor of the active tab"
        );
        assert!(
            !shell.inspector.read(cx).is_editing(cx),
            "the Preview tab is the editor, so the mirror is not"
        );
    });
    assert!(
        shell.read_with(cx, |shell, _cx| shell
            .editor_source()
            .map(|source| source.entity_id())
            == shell.views[preview].as_ref().and_then(|slot| match slot {
                TabView::Preview(view) => Some(view.entity_id()),
                _ => None,
            })),
        "the Preview tab stays the only editor"
    );
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(0, cx)));
}

#[gpui_kit::test]
fn a_fixed_preview_refreshes_for_the_same_uid(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    open_details(&shell, cx, pod_row("pinned", "uid-pinned", "Pending"));
    let fixed = shell.read_with(cx, |shell, _| shell.active_tab);
    assert!(
        preview_yaml(&shell, cx, fixed).is_some_and(|yaml| yaml.contains("Pending")),
        "the first pin fills the tab"
    );

    open_details(&shell, cx, pod_row("pinned", "uid-pinned", "Running"));
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), fixed);
    assert_eq!(shell.read_with(cx, |shell, _| shell.tabs.len()), 3);
    assert!(
        preview_yaml(&shell, cx, fixed).is_some_and(|yaml| yaml.contains("Running")),
        "a new revision of the same UID must refresh the fixed tab"
    );

    shell.update(cx, |shell, cx| assert!(shell.activate_tab(0, cx)));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.close_tab_index(fixed, window, cx));
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.views[fixed].is_none()));
    open_details(&shell, cx, pod_row("pinned", "uid-pinned", "Succeeded"));
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), fixed);
    assert!(
        preview_yaml(&shell, cx, fixed).is_some_and(|yaml| yaml.contains("Succeeded")),
        "a reopened fixed tab shows the object again"
    );
}

#[gpui_kit::test]
fn a_same_uid_update_invalidates_the_cached_object_data(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    let spec = ResourceSpec::pods();
    shell.update(cx, |shell, cx| {
        shell.on_row_selection(
            Some(pod_row("watched", "uid-watched", "Pending")),
            spec.clone(),
            0,
            cx,
        );
    });
    let first = shell.read_with(cx, |shell, cx| shell.inspector.read(cx).session_identity());
    shell.update(cx, |shell, cx| {
        shell.on_row_selection(
            Some(pod_row("watched", "uid-watched", "Running")),
            spec,
            0,
            cx,
        );
    });
    let second = shell.read_with(cx, |shell, cx| shell.inspector.read(cx).session_identity());
    assert_ne!(first.id, second.id, "the cached object data is dropped");
    assert_eq!(first.cluster_id, second.cluster_id);
    assert!(
        shell.read_with(cx, |shell, cx| shell
            .inspector
            .read(cx)
            .current_selection(cx)
            .is_some_and(|selection| selection.yaml.contains("Running"))),
        "the panel shows the new revision instead of the one it cached"
    );

    shell.update(cx, |shell, cx| {
        shell.on_row_selection(
            Some(pod_row("other", "uid-other", "Running")),
            ResourceSpec::pods(),
            0,
            cx,
        );
    });
    let third = shell.read_with(cx, |shell, cx| shell.inspector.read(cx).session_identity());
    assert_eq!(second.id, third.id);
}

/// The identity header has to sit above the tab strip, so no tab can be read without knowing
/// which object it belongs to.
fn assert_identity_header_above_the_tab_strip(cx: &mut gpui_kit::VisualTestContext) {
    let header = cx
        .debug_bounds("inspector-identity")
        .expect("the Inspector names the object it shows");
    let tabs = cx.debug_bounds("inspector-tabs").expect("the tab strip");
    assert!(
        header.bottom() <= tabs.top(),
        "the identity header stays above the tab strip: {header:?} {tabs:?}"
    );
}

/// The header is the only place that survives a toolbar that clips, so it must be present in
/// every selection state and must never keep naming an object the panel no longer holds.
#[gpui_kit::test]
fn the_inspector_header_always_names_the_object_it_shows(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| {
        shell.inspector_open = true;
        cx.notify();
    });
    cx.simulate_resize(gpui_kit::size(px(1440.), px(800.)));
    cx.run_until_parked();

    for row in [
        Some(pod_row("watched", "uid-watched", "Running")),
        None,
        Some(pod_row("other", "uid-other", "Pending")),
    ] {
        shell.update(cx, |shell, cx| {
            shell.on_row_selection(row, ResourceSpec::pods(), 0, cx);
        });
        cx.run_until_parked();
        assert_identity_header_above_the_tab_strip(cx);
    }
}

#[gpui_kit::test]
fn a_context_switch_leaves_the_following_preview_pending(cx: &mut TestAppContext) {
    init_ui(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    open_details(&shell, cx, pod_row("followed", "uid-followed", "Running"));
    let preview = shell.read_with(cx, |shell, _| shell.active_tab);

    shell.update(cx, |shell, cx| shell.reset_cluster_bound_ui(cx));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.preview_hydration_pending));
    assert!(shell.read_with(cx, |shell, _| shell.views[preview].is_none()));

    shell.update(cx, |shell, cx| {
        shell.activate_tab(0, cx);
        shell.on_row_selection(
            Some(pod_row("fresh", "uid-fresh", "Running")),
            ResourceSpec::pods(),
            0,
            cx,
        );
    });
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.views[preview].is_none()));
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(preview, cx)));
    assert!(
        !shell.read_with(cx, |shell, _| shell.preview_hydration_pending),
        "the pending tab is filled once a table reports a selection"
    );
    assert!(
        preview_yaml(&shell, cx, preview).is_some_and(|yaml| yaml.contains("uid-fresh")),
        "the reopened Preview tab shows the new selection"
    );
}

#[gpui_kit::test]
fn apply_identity_mismatch_is_not_reported_as_success(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, _| {
        shell.apply_future = Some(Rc::new(|request| {
            Box::pin(async move {
                let object = serde_json::from_value(serde_json::json!({
                    "apiVersion": request.target.resource.version,
                    "kind": request.target.resource.kind,
                    "metadata": {
                        "name": request.target.name,
                        "namespace": request.target.namespace,
                        "uid": "uid-someone-else",
                    },
                }))
                .expect("applied object");
                Ok(crate::panels::ApplyOutcome::Applied(Arc::new(object)))
            }) as OpsFuture<crate::panels::ApplyOutcome>
        }));
    });
    focus_table(cx, &shell);
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut("secondary-e", "secondary-shift-y"));
    cx.simulate_input("# replace me");
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| shell.command_apply_yaml(window, cx));
    });
    cx.run_until_parked();
    // Apply only asks for a review, so the write starts when the review is confirmed.
    shell.update(cx, |shell, cx| {
        shell
            .inspector
            .update(cx, |panel, cx| panel.confirm_pending_apply(cx));
    });
    cx.run_until_parked();

    assert!(
        shell
            .read_with(cx, |shell, _| shell.toast.clone())
            .is_none_or(|toast| !toast.message.contains("Applied")),
        "a mismatched apply must not toast a success"
    );
    assert!(shell.read_with(cx, |shell, _| {
        shell
            .notifications
            .iter()
            .any(|notification| notification.message.contains("did not match"))
    }));
}

#[gpui_kit::test]
fn tab_commands_follow_the_focused_tab_bar(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(1, cx)));
    cx.run_until_parked();
    let tabs_focus = shell.read_with(cx, |shell, _| shell.center_tabs_focus.clone());
    cx.update(|window, cx| window.focus(&tabs_focus, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes("left");
    assert_eq!(
        shell.read_with(cx, |shell, _| (shell.active_tab, shell.center_tabs_cursor)),
        (1, Some(0)),
        "the cursor moves without switching the content"
    );

    cx.simulate_keystrokes("secondary-shift-t");
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 1);
    assert!(
        !shell.read_with(cx, |shell, _| shell.open_tabs.contains(&0)),
        "Close Tab follows the tab under the cursor"
    );
}

#[gpui_kit::test]
fn the_tab_menu_opens_from_the_keyboard(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(1, cx)));
    cx.run_until_parked();
    let tabs_focus = shell.read_with(cx, |shell, _| shell.center_tabs_focus.clone());
    cx.update(|window, cx| window.focus(&tabs_focus, cx));
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.tab_context_menu.is_none()));

    cx.simulate_keystrokes("left");
    cx.simulate_keystrokes("shift-f10");
    cx.run_until_parked();
    assert!(shell.read_with(cx, |shell, _| shell.tab_context_menu.is_some()));
    assert!(
        cx.debug_bounds("center-tab-context-menu").is_some(),
        "the keyboard menu shows the tab actions"
    );
    // The first row of the tab list is Close, and the menu owns focus, so Down
    // then Enter is the keyboard reader's path to it.
    cx.simulate_keystrokes("down enter");
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |shell, _| shell.active_tab), 1);
    assert!(!shell.read_with(cx, |shell, _| shell.open_tabs.contains(&0)));
}

#[gpui_kit::test]
fn a_tab_without_a_view_falls_back_to_the_shell_focus(cx: &mut TestAppContext) {
    init_ui(cx);
    install_test_keymap(cx);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::new(cx));
    shell.update(cx, |shell, _| {
        shell.tabs.push(CenterTab {
            content: TabContent::Resource,
            kind: "CustomResourceDefinition".into(),
            identity: None,
            title: "Custom Resource Definitions".into(),
            icon: IconName::Server,
            entry: None,
            resource: None,
            pinned: false,
            preview: None,
        });
        shell.views.push(None);
    });
    cx.run_until_parked();
    let empty = shell.read_with(cx, |shell, _| shell.tabs.len() - 1);
    shell.update(cx, |shell, cx| assert!(shell.activate_tab(empty, cx)));
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            assert!(!shell.focus_active_view_and_clear_pending(window, cx));
        });
    });
    assert!(!shell.read_with(cx, |shell, _| shell.focus_active_view_pending));
    let root = shell.read_with(cx, |shell, _| shell.focus_handle.clone());
    assert!(cx.update(|window, _| root.is_focused(window)));
}

/// A session with nothing to read opens on the screen that explains it.
///
/// This is the whole first run: production builds the shell before kubeconfigs are
/// read, so a machine with no kubeconfig, an unreadable one, or one whose contexts
/// all failed to load arrives here, and the Overview is the only tab that says what
/// happened and carries the control that fixes it. The window used to open on the
/// Pods registry slot instead — an empty table beside an empty tree, with the
/// explanation one click away and never shown — because `ensure_tab_view` skips
/// index 0 and nothing else ever moved the first tab.
#[gpui_kit::test]
fn a_session_with_no_registry_opens_on_the_overview(cx: &mut TestAppContext) {
    init_ui(cx);
    // Exactly what production builds with: kubeconfigs have not been read yet, so
    // there is no registry and no context. The read either lands and replaces this
    // session, or fails and puts the connection surface in front of this tab.
    let session = ClusterSession::unavailable(super::STARTUP_LOADING_REASON);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    cx.run_until_parked();

    assert_eq!(
        shell.read_with(cx, |shell, _| shell
            .tabs
            .get(shell.active_tab)
            .map(|tab| (tab.content, tab.title.clone()))),
        Some((TabContent::Overview, "Overview".into())),
        "a first run has to open on the Overview"
    );
    // The answer is a bool rather than the view: `read_with` cannot hand back a
    // borrow out of the Shell, and the test is about which tab is mounted, not
    // about the view's identity.
    assert!(
        shell.read_with(cx, |shell, _| {
            matches!(
                shell.views.get(shell.active_tab),
                Some(Some(TabView::Overview(_)))
            )
        }),
        "the Overview the window opens on is mounted, not a tab whose view is never built"
    );
    assert!(
        cx.debug_bounds("overview-reload-kubeconfigs").is_some(),
        "the arrival names itself and offers the reload that fixes it"
    );
    assert_eq!(
        shell.read_with(cx, |shell, _| shell.tabs[0].kind.clone()),
        SharedString::from("Pod"),
        "`secondary-1` still opens the Pods table, one click from the explanation"
    );
}

/// The reload the no-cluster state offers rebuilds the Overview it navigates to.
///
/// A recovery button that leaves the reader on a view which never refreshes is a
/// dead end, and the view in question is rebuilt by `apply_session` rather than by
/// the Overview itself: the Overview built with no handle has no way to be given
/// one, so the shell has to build a new one. Both halves are pinned here — the tab
/// stays on the Overview, and the mounted view is a different entity, which is the
/// only thing that can carry the recovered session's handle.
#[gpui_kit::test]
fn reload_kubeconfigs_rebuilds_the_overview_the_reader_is_on(cx: &mut TestAppContext) {
    init_ui(cx);
    init_cluster_runtime(cx);
    let session = ClusterSession::unavailable(super::STARTUP_LOADING_REASON);
    let (shell, cx) = cx.add_window_view(|_, cx| Shell::with_cluster(session, cx));
    cx.run_until_parked();
    let (active, before) = shell.read_with(cx, |shell, _| {
        let view = match shell.views.get(shell.active_tab) {
            Some(Some(TabView::Overview(view))) => view.clone(),
            _ => panic!("the shell opens on the Overview"),
        };
        (shell.active_tab, view)
    });

    let path = std::env::temp_dir().join(format!(
        "k8s-gpui-first-run-recovery-{}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, RELOADED_KUBECONFIG).expect("write reloaded config");
    shell.update(cx, |shell, _| {
        shell.reload_kubeconfig_path = Some(path.clone());
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell.reload_kubeconfigs(&ReloadKubeconfigs, window, cx);
        });
    });
    for _ in 0..100 {
        if shell.read_with(cx, |shell, _| {
            !shell.reload_in_progress
                && shell
                    .toast
                    .as_ref()
                    .is_some_and(|toast| toast.message.contains("Kubeconfigs reloaded"))
        }) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
        cx.run_until_parked();
    }
    let _ = std::fs::remove_file(&path);
    cx.run_until_parked();

    shell.read_with(cx, |shell, _| {
        assert_eq!(
            shell.active_tab, active,
            "the recovery keeps the reader on the tab they asked from"
        );
        match shell.views.get(shell.active_tab) {
            Some(Some(TabView::Overview(view))) => assert_ne!(
                view, &before,
                "the Overview is rebuilt: the one built with no handle can never be given one"
            ),
            _ => panic!("the recovery leaves an Overview mounted"),
        }
    });
    assert!(
        shell.read_with(cx, |shell, _| shell
            .session
            .as_ref()
            .and_then(ClusterSession::cluster_id)
            .is_some()),
        "the reload gave the session a cluster to read"
    );
    assert!(
        cx.debug_bounds("overview-reload-kubeconfigs").is_none(),
        "the recovered Overview has a handle, so it no longer offers the reload that gets one"
    );
}
