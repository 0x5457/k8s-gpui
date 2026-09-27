use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, SharedString};
use k8s_core::cluster::{ClusterId, ClusterRegistry};
use k8s_core::cluster_data::ClusterDataSource;
use k8s_core::ops::LogOptions;
use kube_core::{ApiResource, DynamicObject};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::{OnceCell, mpsc};

pub use k8s_core::ops::{ApplyOutcome, DescribeData};

pub type OpsFuture<T> = Pin<Box<dyn Future<Output = Result<T, String>>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRef {
    pub resource: ApiResource,
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorSelection {
    pub object: ObjectRef,
    pub yaml: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceAccountTarget {
    pub namespace: String,
    pub name: String,
}

const POD_NAMESPACE_MISSING: &str = "This Pod has no namespace, so its Service Account cannot be opened. Refresh the Pod list, then try again.";
const POD_NAMESPACE_INVALID: &str = "This Pod has an invalid namespace, so its Service Account cannot be opened. Refresh the Pod list, then try again.";
const SERVICE_ACCOUNT_NAME_INVALID: &str =
    "This Pod has an invalid Service Account name. Refresh the Pod list, then try again.";

fn valid_service_account_component(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|character| character.is_whitespace())
}

fn service_account_target_from_value(object: &Value) -> Result<ServiceAccountTarget, &'static str> {
    let namespace = match object.pointer("/metadata/namespace") {
        Some(Value::String(namespace)) => namespace,
        None | Some(Value::Null) => return Err(POD_NAMESPACE_MISSING),
        Some(_) => return Err(POD_NAMESPACE_INVALID),
    };
    if !valid_service_account_component(namespace) {
        return Err(POD_NAMESPACE_INVALID);
    }
    let name = match object.pointer("/spec/serviceAccountName") {
        None | Some(Value::Null) => "default",
        Some(Value::String(name)) if valid_service_account_component(name) => name,
        Some(_) => return Err(SERVICE_ACCOUNT_NAME_INVALID),
    };
    Ok(ServiceAccountTarget {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
    })
}

pub fn pod_service_account_target(
    object: &DynamicObject,
) -> Result<ServiceAccountTarget, &'static str> {
    let Ok(value) = serde_json::to_value(object) else {
        return Err(POD_NAMESPACE_INVALID);
    };
    service_account_target_from_value(&value)
}

pub trait InspectorSource: 'static {
    fn describe(&self, object: &ObjectRef) -> OpsFuture<DescribeData>;
    fn events(&self, object: &ObjectRef) -> OpsFuture<Vec<DynamicObject>>;
}

pub fn gvr_label(resource: &ApiResource) -> String {
    format!("{}, Resource={}", resource.api_version, resource.plural)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRequest {
    pub namespace: Option<SharedString>,
    pub name: SharedString,
    pub containers: Vec<SharedString>,
}

#[derive(Clone, Debug)]
pub enum LogEvent {
    Line(String),
    Ended(String),
}

pub const LOG_EVENT_MAX_BYTES: usize = 16 * 1024;

impl LogEvent {
    pub(crate) fn bounded(self) -> Self {
        match self {
            Self::Line(line) => Self::Line(bounded_text(line)),
            Self::Ended(reason) => Self::Ended(bounded_text(reason)),
        }
    }
}

fn bounded_text(value: String) -> String {
    if value.len() <= LOG_EVENT_MAX_BYTES {
        return value;
    }
    let mut end = LOG_EVENT_MAX_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

pub type LogSink = mpsc::Sender<LogEvent>;

pub trait LogSubscription: 'static {
    fn cancel(&mut self);
}

pub type LogFactory = Rc<dyn Fn(LogRequest, LogOptions, LogSink) -> Box<dyn LogSubscription>>;

pub enum InspectorUpdate {
    Selection(Option<InspectorSelection>),
    Yaml(Option<String>),
}

type InspectorBindingApply = Rc<dyn Fn(InspectorUpdate, &mut App)>;

#[derive(Clone)]
pub struct InspectorBinding {
    apply: InspectorBindingApply,
}

impl InspectorBinding {
    pub fn new(apply: impl Fn(InspectorUpdate, &mut App) + 'static) -> Self {
        Self {
            apply: Rc::new(apply),
        }
    }

    pub fn apply(&self, update: InspectorUpdate, cx: &mut App) {
        (self.apply)(update, cx);
    }
}

pub trait InspectorBindingInput {
    fn into_binding(self) -> Option<InspectorBinding>;
}

impl InspectorBindingInput for InspectorBinding {
    fn into_binding(self) -> Option<InspectorBinding> {
        Some(self)
    }
}

impl InspectorBindingInput for Option<InspectorBinding> {
    fn into_binding(self) -> Option<InspectorBinding> {
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceSpec {
    pub kind: SharedString,
    pub label: SharedString,
    pub namespaced: bool,
    pub resource: Option<ApiResource>,
}

impl ResourceSpec {
    pub fn new(
        kind: impl Into<SharedString>,
        label: impl Into<SharedString>,
        namespaced: bool,
    ) -> Self {
        Self {
            kind: kind.into(),
            label: label.into(),
            namespaced,
            resource: None,
        }
    }

    pub fn with_resource(mut self, resource: ApiResource) -> Self {
        self.resource = Some(resource);
        self
    }

    pub fn pods() -> Self {
        Self::new("Pod", "Pods", true).with_resource(pods_resource())
    }

    pub fn service_accounts() -> Self {
        Self::new("ServiceAccount", "Service Accounts", true)
            .with_resource(service_accounts_resource())
    }
}

pub fn pods_resource() -> ApiResource {
    ApiResource {
        group: String::new(),
        version: "v1".to_owned(),
        api_version: "v1".to_owned(),
        kind: "Pod".to_owned(),
        plural: "pods".to_owned(),
    }
}

pub fn service_accounts_resource() -> ApiResource {
    ApiResource {
        group: String::new(),
        version: "v1".to_owned(),
        api_version: "v1".to_owned(),
        kind: "ServiceAccount".to_owned(),
        plural: "serviceaccounts".to_owned(),
    }
}

#[derive(Clone)]
pub struct ClusterCache {
    pub(crate) root: PathBuf,
    pub(crate) cluster_id: ClusterId,
    pub(crate) enabled: bool,
    pub(crate) uid: Arc<OnceCell<String>>,
    pub(crate) service: Option<ClusterDataSource>,
}

#[derive(Clone)]
pub enum ClusterSession {
    Ready {
        registry: Arc<ClusterRegistry>,
        cluster: ClusterId,
        handle: Handle,
        cache: Option<Arc<ClusterCache>>,
    },
    Unavailable {
        reason: String,
        registry: Option<Arc<ClusterRegistry>>,
        handle: Option<Handle>,
    },
}

#[path = "table_view/input.rs"]
mod input;

pub use input::TextInput;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(service_account: Value) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": "web",
                "namespace": "team-a",
                "uid": "pod-uid",
            },
            "spec": { "serviceAccountName": service_account },
        })
    }

    #[test]
    fn service_account_defaults_when_the_pod_field_is_missing_or_null() {
        let missing = json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "web", "namespace": "team-a" },
        });
        for object in [missing, pod(json!(null))] {
            assert_eq!(
                service_account_target_from_value(&object),
                Ok(ServiceAccountTarget {
                    namespace: "team-a".to_owned(),
                    name: "default".to_owned(),
                })
            );
        }
    }

    #[test]
    fn service_account_uses_the_pod_namespace_and_explicit_name() {
        assert_eq!(
            service_account_target_from_value(&pod(json!("runner"))),
            Ok(ServiceAccountTarget {
                namespace: "team-a".to_owned(),
                name: "runner".to_owned(),
            })
        );
    }

    #[test]
    fn service_account_rejects_blank_and_non_string_values() {
        for value in [json!(""), json!("   "), json!(7), json!(false)] {
            assert_eq!(
                service_account_target_from_value(&pod(value)),
                Err(SERVICE_ACCOUNT_NAME_INVALID)
            );
        }
    }

    #[test]
    fn service_account_rejects_missing_or_invalid_pod_namespaces() {
        for value in [json!(null), json!(""), json!("   "), json!(9)] {
            let mut object = pod(json!("runner"));
            object["metadata"]["namespace"] = value;
            assert!(service_account_target_from_value(&object).is_err());
        }
        let mut object = pod(json!("runner"));
        object["metadata"]
            .as_object_mut()
            .expect("metadata")
            .remove("namespace");
        assert_eq!(
            service_account_target_from_value(&object),
            Err(POD_NAMESPACE_MISSING)
        );
    }

    #[test]
    fn log_events_are_bounded_on_utf8_boundaries() {
        let event = LogEvent::Line("你".repeat(LOG_EVENT_MAX_BYTES)).bounded();
        let LogEvent::Line(line) = event else {
            unreachable!();
        };
        assert!(line.len() <= LOG_EVENT_MAX_BYTES);
        assert!(line.is_char_boundary(line.len()));
    }
}
