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

pub use crate::session::{ClusterSession, ResourceSpec, TextInput, pods_resource};
pub use actions::{
    DeleteSelection, EditYaml, OpenDetails, OpenRowActions, Refresh, SelectNext, SelectNextColumn,
    SelectPrevious, SelectPreviousColumn, SortSelectedColumn,
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
