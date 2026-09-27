//! UI contracts for Dock terminal sessions and port forwards.
//!
//! k8s-app supplies factories for terminal creation, kubeconfig setup, and event delivery.
//! The Dock manages the session list, activation, and closing.

use std::rc::Rc;

use gpui::{AnyView, App, SharedString, Window};

pub const ALL_NAMESPACES: &str = "All Namespaces";

/// Selects the terminal title and factory route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalKind {
    /// Local shell with a temporary kubeconfig for the current context.
    Local,
    /// Interactive cluster exec session.
    Exec {
        namespace: String,
        pod: String,
        container: Option<String>,
    },
}

impl TerminalKind {
    /// Short title for tabs and the session switcher.
    pub fn title(&self, cluster: Option<&str>) -> SharedString {
        self.title_for_namespace(cluster, None)
    }

    pub fn title_for_namespace(
        &self,
        cluster: Option<&str>,
        namespace: Option<&str>,
    ) -> SharedString {
        let cluster = cluster.unwrap_or("cluster");
        match self {
            Self::Local => format!(
                "{cluster}/{}",
                namespace
                    .filter(|value| !value.is_empty())
                    .unwrap_or(ALL_NAMESPACES)
            )
            .into(),
            Self::Exec {
                namespace,
                pod,
                container,
            } => match container {
                Some(container) => format!("{cluster}/{namespace}/{pod}:{container}").into(),
                None => format!("{cluster}/{namespace}/{pod}").into(),
            },
        }
    }

    /// Returns true for cluster exec sessions.
    pub fn is_exec(&self) -> bool {
        matches!(self, Self::Exec { .. })
    }
}

/// Request to open a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRequest {
    pub kind: TerminalKind,
    /// Current kubeconfig context.
    pub context: Option<String>,
    pub namespace: Option<String>,
}

impl TerminalRequest {
    /// Request for a second pane split off this session. An exec session splits into the same
    /// pod and container, because that is the target the user is looking at. Every other kind
    /// splits into a local shell in the same cluster and namespace, so a split never silently
    /// jumps to a namespace the pane it was split from does not use.
    pub fn split_request(&self) -> Self {
        let kind = match &self.kind {
            kind @ TerminalKind::Exec { .. } => kind.clone(),
            TerminalKind::Local => TerminalKind::Local,
        };
        Self {
            kind,
            context: self.context.clone(),
            namespace: self.namespace.clone(),
        }
    }
}

/// Gives keyboard focus to the terminal canvas.
pub type ActivateTerminal = Box<dyn Fn(&mut Window, &mut App)>;

/// A created terminal view and its focus action.
pub struct TerminalInstance {
    /// Terminal canvas. The Dock renders the title bar.
    pub view: AnyView,
    /// Moves keyboard focus to the terminal.
    pub activate: ActivateTerminal,
}

/// Terminal session events from the process and OSC title sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    Title(String),
    Exited {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

impl TerminalEvent {
    /// Returns a user-facing exit status.
    pub fn exit_label(&self) -> Option<String> {
        match self {
            Self::Title(_) => None,
            Self::Exited { code, signal } => Some(match (code, signal) {
                (Some(code), _) => format!("Exited with code {code}"),
                (None, Some(signal)) => format!("Terminated by signal {signal}"),
                (None, None) => "Exited".to_owned(),
            }),
        }
    }
}

/// Delivers terminal events from a factory to the Dock.
pub type TerminalEventSink = Box<dyn Fn(TerminalEvent, &mut App) + 'static>;

/// Creates local or cluster terminal instances.
pub type TerminalFactory =
    Rc<dyn Fn(TerminalRequest, TerminalEventSink, &mut App) -> Result<TerminalInstance, String>>;

/// Stops a port forward when dropped.
pub trait ForwardHandle: 'static {
    fn stop(&mut self);
}

pub type ForwardBinding = tokio::sync::oneshot::Receiver<Result<u16, String>>;

/// Request for a port forward.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForwardRequest {
    /// Current kubeconfig context.
    pub context: Option<String>,
    pub namespace: Option<String>,
    pub name: SharedString,
    pub remote_port: u16,
    /// Local port the user asked for. `None` lets the system pick a free port, which is also
    /// what a request built before this field existed means.
    pub local_port: Option<u16>,
}

impl ForwardRequest {
    /// The local port this request asks for, if the user named one.
    pub fn requested_local_port(&self) -> Option<u16> {
        self.local_port
    }
}

/// Reads the local port a user typed. Empty text means the system assigns a free port, so the
/// field can stay empty and a forward still starts.
pub fn parse_local_port(text: &str) -> Result<Option<u16>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let port: u32 = text
        .parse()
        .map_err(|_| format!("{text} is not a local port. Enter a number from 1 to 65535."))?;
    if port == 0 || port > u32::from(u16::MAX) {
        return Err(format!(
            "{port} is not a usable local port. Enter a number from 1 to 65535."
        ));
    }
    Ok(Some(port as u16))
}

/// True when nothing is listening on `port` yet, so a requested local port can bind.
///
/// This is a pre-flight hint for the dialog. The probe releases the port immediately, so the
/// forward still races for it; a port that is taken by then is reported by the forward itself
/// instead of being chosen silently.
pub fn local_port_available(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

/// Accepted port forward with asynchronous binding and error results.
pub struct StartedForward {
    pub handle: Box<dyn ForwardHandle>,
    pub binding: ForwardBinding,
    /// Runtime port forwarding errors.
    pub errors: tokio::sync::mpsc::UnboundedReceiver<String>,
}

/// Starts a port forward and returns binding results asynchronously.
pub type PortForwardFactory =
    Rc<dyn Fn(ForwardRequest, &mut App) -> Result<StartedForward, String>>;

/// Terminal and port forward services for the active cluster.
#[derive(Clone)]
pub struct TerminalServices {
    pub terminals: TerminalFactory,
    pub forwards: PortForwardFactory,
    /// Current kubeconfig context.
    pub context: Option<String>,
    pub namespace: Option<String>,
}

impl TerminalServices {
    pub fn is_available(&self) -> bool {
        self.context.is_some()
    }

    pub fn terminal_request(&self, kind: TerminalKind) -> TerminalRequest {
        TerminalRequest {
            kind,
            context: self.context.clone(),
            namespace: self.namespace.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_titles_include_namespace_scope() {
        assert_eq!(
            TerminalKind::Local
                .title_for_namespace(Some("kind-dev"), Some("team-a"))
                .as_ref(),
            "kind-dev/team-a"
        );
        assert_eq!(
            TerminalKind::Local
                .title_for_namespace(Some("kind-dev"), None)
                .as_ref(),
            "kind-dev/All Namespaces"
        );
    }

    fn request(kind: TerminalKind) -> TerminalRequest {
        TerminalRequest {
            kind,
            context: Some("kind-dev".to_owned()),
            namespace: Some("team-a".to_owned()),
        }
    }

    #[test]
    fn exec_split_keeps_the_pod_target() {
        let exec = request(TerminalKind::Exec {
            namespace: "team-a".to_owned(),
            pod: "web-0".to_owned(),
            container: Some("app".to_owned()),
        });
        assert_eq!(exec.split_request(), exec);
        assert!(exec.kind.is_exec());
    }

    #[test]
    fn local_split_keeps_the_session_scope() {
        let local = request(TerminalKind::Local).split_request();
        assert_eq!(local.kind, TerminalKind::Local);
        assert_eq!(local.namespace.as_deref(), Some("team-a"));
        assert_eq!(local.context.as_deref(), Some("kind-dev"));
    }

    fn forward_request(local_port: Option<u16>) -> ForwardRequest {
        ForwardRequest {
            context: Some("kind-dev".to_owned()),
            namespace: Some("team-a".to_owned()),
            name: "web-0".into(),
            remote_port: 8080,
            local_port,
        }
    }

    #[test]
    fn an_empty_local_port_means_the_system_assigns_one() {
        assert_eq!(parse_local_port(""), Ok(None));
        assert_eq!(parse_local_port("   "), Ok(None));
        assert_eq!(parse_local_port("8080"), Ok(Some(8080)));
        assert_eq!(parse_local_port(" 65535 "), Ok(Some(65535)));
        assert_eq!(forward_request(None).requested_local_port(), None);
        assert_eq!(
            forward_request(Some(8081)).requested_local_port(),
            Some(8081)
        );
    }

    #[test]
    fn a_local_port_outside_the_port_range_is_refused_with_the_range() {
        for text in ["0", "65536", "99999"] {
            let error = parse_local_port(text).expect_err("out of range");
            assert!(
                error.contains("1 to 65535"),
                "the message must name the range: {error}"
            );
        }
        let error = parse_local_port("http").expect_err("not a number");
        assert!(error.contains("not a local port"), "message: {error}");
    }

    #[test]
    fn a_free_local_port_is_available_and_an_occupied_one_is_not() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupy a port");
        let port = occupied.local_addr().expect("local address").port();
        assert!(!local_port_available(port), "a bound port is not available");
        drop(occupied);
        assert!(local_port_available(port), "a released port is available");
    }
}
