//! Bridges the resource table state machine to GPUI.
//!
//! `ResourceSource` sends events to `TableHost`. The host updates the machine
//! and runs its effects. `PodsView` renders snapshots and sends user actions as
//! table events.

mod actions;
pub mod cache;
mod columns;
mod host;
mod kube_source;
mod source;
mod view;

use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;

pub use crate::session::{ClusterSession, ResourceSpec, TextInput, pods_resource};

/// A bounded or unbounded channel to the UI thread, as one type so a caller
/// never has to branch on which of the two it was handed.
///
/// The two shapes existed as two enums with these same variants: one for the
/// async sources, one for the thread-backed fake. Only the send strategy
/// differed, so the strategy is a method rather than a type.
#[derive(Clone)]
pub(crate) enum EventSender {
    Bounded(mpsc::Sender<SourceEvent>),
    Unbounded(UnboundedSender<SourceEvent>),
}

impl From<mpsc::Sender<SourceEvent>> for EventSender {
    fn from(events: mpsc::Sender<SourceEvent>) -> Self {
        Self::Bounded(events)
    }
}

impl From<UnboundedSender<SourceEvent>> for EventSender {
    fn from(events: UnboundedSender<SourceEvent>) -> Self {
        Self::Unbounded(events)
    }
}

impl EventSender {
    /// Sends without waiting: on a full bounded queue the event is dropped,
    /// which is what a redraw storm wants.
    pub(crate) fn try_send(&self, event: SourceEvent) -> bool {
        match self {
            Self::Bounded(events) => events.try_send(event).is_ok(),
            Self::Unbounded(events) => events.send(event).is_ok(),
        }
    }

    /// Sends from a thread with no runtime to await on, blocking for room.
    pub(crate) fn send_blocking(&self, event: SourceEvent) -> bool {
        match self {
            Self::Bounded(events) => events.blocking_send(event).is_ok(),
            Self::Unbounded(events) => events.send(event).is_ok(),
        }
    }
}
pub use actions::{
    DeleteSelection, EditYaml, OpenDetails, OpenRowActions, Refresh, SelectNext, SelectNextColumn,
    SelectPrevious, SelectPreviousColumn, SortSelectedColumn, ToggleProblemsOnly,
};
pub use cache::ClusterCache;
pub use columns::{ResourceColumn, columns_for, is_known_kind, pod_columns};
pub use host::{CachedRows, PendingOp, SourceFactory, TableHost, TableStatus};
pub use k8s_core::projection::Row;
pub use kube_source::{CatalogHandle, ClusterHandle, KubeSource};
pub use source::{
    ChurnHandle, FakeSource, FakeSourceConfig, ObjectOps, ResourceSource, SourceEvent, Subscription,
};
pub use view::{DeleteTarget, ExecTarget, PodsView, PortForwardTarget, ScaleTarget};
