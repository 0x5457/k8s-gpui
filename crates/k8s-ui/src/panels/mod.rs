//! Inspector and Dock panels.
//!
//! Shell owns layout and visibility. Panels own their headers, tabs, and content.
pub(crate) mod common;
pub mod dock;
pub mod forwards;
pub mod helm;
pub mod inspector;
pub mod inspector_data;
pub mod logs;
pub mod metrics;
pub mod overview;
pub mod search;
pub mod settings_view;
pub mod terminal;

pub use dock::{DockPanel, ForwardId, ForwardPhase, ForwardSnapshot, ForwardSummary};
pub use forwards::{ForwardsView, NewForwardCallback};
pub use helm::{HelmAction, HelmServices, HelmView};
pub use inspector::{ApplyVerdict, InspectorPanel, InspectorSelection, InspectorTab};
pub use inspector_data::{ApplyOutcome, DescribeData, InspectorSource, ObjectRef};
pub use logs::{LogEvent, LogFactory, LogPhase, LogRequest, LogSubscription, TailLines};
pub use metrics::{MetricsHandle, MetricsTarget};
pub use overview::{OverviewHandle, OverviewView};
pub use search::{SEARCH_FAILED, SEARCH_UNAVAILABLE, SearchExecutor, SearchView};
pub use settings_view::{Capability, SettingsView};
pub use terminal::{
    ActivateTerminal, ForwardHandle, ForwardRequest, PortForwardFactory, StartedForward,
    TerminalEvent, TerminalEventSink, TerminalFactory, TerminalInstance, TerminalKind,
    TerminalRequest, TerminalServices,
};

/// The empty state, for the faces that live outside `panels`.
///
/// The YAML editor had its own copy, and the two copies of one message ended up
/// disagreeing on punctuation and on the icon size, which is the failure this
/// re-export removes.
pub(crate) use common::empty_state;
