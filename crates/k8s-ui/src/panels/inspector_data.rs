//! Inspector data types for object references, Describe and Events payloads, and on-demand sources.
//! The table layer converts Kubernetes data at the boundary.
//! Panel-owned async work uses `Pin<Box<dyn Future>>`.

use kube_core::ApiResource;

pub use crate::session::{InspectorSelection, InspectorSource, ObjectRef, OpsFuture};
pub use k8s_core::cluster::ClusterId;

/// Reuses the core operation payload types.
pub use k8s_core::ops::{ApplyOutcome, DescribeData};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct InspectorSession {
    pub id: u64,
    pub cluster_id: Option<ClusterId>,
}

pub type SessionIdentity = InspectorSession;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyTarget {
    pub session: Option<InspectorSession>,
    pub resource: ApiResource,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
}

impl ApplyTarget {
    pub fn from_object(object: ObjectRef, session: Option<InspectorSession>) -> Self {
        Self {
            session,
            resource: object.resource,
            namespace: object.namespace,
            name: object.name,
            uid: object.uid,
        }
    }

    pub fn object_ref(&self) -> ObjectRef {
        ObjectRef {
            resource: self.resource.clone(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            uid: self.uid.clone(),
        }
    }

    pub fn object(&self) -> ObjectRef {
        self.object_ref()
    }

    pub fn is_complete(&self) -> bool {
        !self.name.is_empty()
            && !self.uid.is_empty()
            && !self.resource.kind.is_empty()
            && !self.resource.version.is_empty()
            && !self.resource.api_version.is_empty()
            && !self.resource.plural.is_empty()
            && self
                .namespace
                .as_deref()
                .is_none_or(|namespace| !namespace.is_empty())
    }

    pub fn cluster_id(&self) -> Option<ClusterId> {
        self.session.and_then(|session| session.cluster_id)
    }

    pub fn session_id(&self) -> Option<u64> {
        self.session.map(|session| session.id)
    }
}

pub type InspectorApplyTarget = ApplyTarget;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyRequest {
    pub id: u64,
    pub target: ApplyTarget,
    pub yaml: String,
}

impl ApplyRequest {
    pub fn new(id: u64, target: ApplyTarget, yaml: String) -> Self {
        Self { id, target, yaml }
    }

    pub fn request_id(&self) -> u64 {
        self.id
    }
}

pub type InspectorApplyRequest = ApplyRequest;
