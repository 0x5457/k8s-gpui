//! Provides Kubernetes object operations, logs, exec, and port forwarding.

use std::cmp::Ordering;
use std::fmt;
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use futures::channel::mpsc as futures_mpsc;
use json_patch::Patch as JsonPatch;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Status as K8sStatus;
use k8s_openapi::apimachinery::pkg::version::Info;
use kube::Client;
use kube::api::{
    Api, ApiResource, AttachParams, AttachedProcess, DeleteParams, ListParams, LogParams,
    ObjectList, Patch, PatchParams, Portforwarder, Preconditions, Request, ValidationDirective,
};
pub use kube::core::DynamicObject;
use kube::core::GroupVersionKind;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::discovery::ResourceEntry;
use crate::history;
use crate::latency::{NON_WATCH_READ_TIMEOUT, with_read_timeout};

/// Terminal size type used by kube.
pub use kube::api::TerminalSize;

/// Field manager used for server-side apply.
pub const FIELD_MANAGER: &str = "k8s-gpui";

/// Annotation used by `kubectl rollout restart`.
pub const RESTART_ANNOTATION: &str = "kubectl.kubernetes.io/restartedAt";

/// Log channel capacity. A slow consumer applies backpressure to the HTTP stream.
pub const TRUNCATED_LOG_LINE: &str = "… [Log line truncated]";
const LOG_CHANNEL_CAPACITY: usize = 1024;

const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
fn format_container_list(containers: &[String]) -> String {
    containers.join(", ")
}

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error("Kubernetes request failed: {0}. Check the cluster connection and try again.")]
    Kube(#[from] kube::Error),

    #[error(
        "Kubernetes request timed out after 30 seconds. Check the cluster connection and try again."
    )]
    Timeout,

    #[error("Failed to build the UID protection patch: {0}. Check the object UID and try again.")]
    PatchBuild(String),

    #[error("Failed to read the log stream: {0}. Check the pod and try again.")]
    LogStreamRead(String),

    #[error("Failed to parse YAML: {0}. Check the YAML syntax and try again.")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("The object has no metadata.name. Add metadata.name and try again.")]
    MissingName,

    #[error("The object has no metadata.uid. Refresh the object and try again.")]
    MissingUid,

    #[error(
        "The object UID `{found}` does not match the expected UID `{expected}`. Refresh the object and try again."
    )]
    UidMismatch { expected: String, found: String },

    #[error(
        "YAML kind {found} does not match the target resource {expected}. Use YAML for {expected}."
    )]
    KindMismatch { found: String, expected: String },

    #[error("{what} supports only Pod resources. Received {resource}. Select a Pod and try again.")]
    UnsupportedResource {
        what: &'static str,
        resource: String,
    },

    #[error(
        "Pod {pod} has multiple containers. Use `--container` to select one: {}",
        format_container_list(containers)
    )]
    ContainerSelectionRequired {
        pod: String,
        containers: Vec<String>,
    },

    #[error(
        "Pod {pod} does not contain container {container}. Use `--container` with one of: {}.",
        format_container_list(containers)
    )]
    ContainerNotFound {
        pod: String,
        container: String,
        containers: Vec<String>,
    },

    #[error(
        "{what} requires a namespace because Pod is a namespaced resource. Set a namespace and try again."
    )]
    NamespaceRequired { what: &'static str },

    #[error("{what} requires a command. Enter a command and try again.")]
    MissingCommand { what: &'static str },

    #[error("Only a TTY session supports resize.")]
    ResizeWithoutTty,

    #[error("The exec connection closed before the exit status arrived. Retry the command.")]
    ExecDisconnected,

    #[error(
        "Port forwarding did not receive a stream for port {port}. The Kubernetes port does not match the request. Retry the request."
    )]
    MissingForwardStream { port: u16 },

    /// The local port the reader asked for could not be opened.
    ///
    /// `port` is the *local* one, which is the number the reader typed and the only one they
    /// can change. This message used to print the remote port under the words "the local port",
    /// which named a number the reader cannot edit.
    #[error(
        "Local port {port} could not be opened: {source}. Choose another local port and try again."
    )]
    BindLocalPort {
        port: u16,
        #[source]
        source: std::io::Error,
    },

    /// The system could not assign a local port, so there is no address to hand back.
    #[error(
        "Could not open a local port for remote port {remote}: {source}. Check that this machine can open local ports, then try again."
    )]
    AssignLocalPort {
        remote: u16,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Clone, Debug)]
pub enum ApplyOutcome {
    Applied(Arc<DynamicObject>),
    Conflict { owners: Vec<String> },
    Unknown { reason: String },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceReadOptions<'a> {
    pub namespace: Option<&'a str>,
    pub name: Option<&'a str>,
    pub label_selector: Option<&'a str>,
    pub field_selector: Option<&'a str>,
}

#[derive(Debug)]
pub enum ResourceData {
    List(ObjectList<DynamicObject>),
    One(Box<DynamicObject>),
}

impl ResourceData {
    pub fn items(&self) -> Vec<&DynamicObject> {
        match self {
            Self::List(list) => list.items.iter().collect(),
            Self::One(object) => vec![object],
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResourceReadError {
    #[error(
        "Resource type `{plural}` does not exist in the cluster. Refresh discovery and try again."
    )]
    ResourceTypeNotFound { plural: String },

    #[error("{kind} `{name}` does not exist. Refresh the resource list and try again.")]
    ObjectNotFound { kind: String, name: String },

    #[error(
        "kubectl {plural} failed with HTTP {code}: {message}. Check permissions and try again."
    )]
    Api {
        plural: String,
        code: u16,
        message: String,
    },

    #[error("kubectl {plural} failed: {message}. Check the cluster connection and try again.")]
    Transport { plural: String, message: String },

    #[error(
        "kubectl {plural} failed: Kubernetes request timed out after 30 seconds. Check the cluster connection and try again."
    )]
    Timeout { plural: String },
}

pub async fn read_resource(
    client: &Client,
    entry: &ResourceEntry,
    options: ResourceReadOptions<'_>,
) -> Result<ResourceData, ResourceReadError> {
    let namespace = if entry.namespaced() {
        options.namespace
    } else {
        None
    };
    let api = api_for(client, &entry.to_api_resource(), namespace);
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            if let Some(name) = options.name {
                return api
                    .get(name)
                    .await
                    .map(|object| ResourceData::One(Box::new(object)))
                    .map_err(|error| resource_read_error(entry, Some(name), error));
            }
            let mut params = ListParams::default();
            if let Some(selector) = options.label_selector {
                params = params.labels(selector);
            }
            if let Some(selector) = options.field_selector {
                params = params.fields(selector);
            }
            api.list(&params)
                .await
                .map(ResourceData::List)
                .map_err(|error| resource_read_error(entry, None, error))
        },
        ResourceReadError::Timeout {
            plural: entry.plural.clone(),
        },
    )
    .await
}

fn resource_read_error(
    entry: &ResourceEntry,
    name: Option<&str>,
    error: kube::Error,
) -> ResourceReadError {
    match error {
        kube::Error::Api(response) if response.code == 404 => match name {
            Some(name) => ResourceReadError::ObjectNotFound {
                kind: entry.kind.clone(),
                name: name.to_string(),
            },
            None => ResourceReadError::ResourceTypeNotFound {
                plural: entry.plural.clone(),
            },
        },
        kube::Error::Api(response) => ResourceReadError::Api {
            plural: entry.plural.clone(),
            code: response.code,
            message: response.message,
        },
        other => ResourceReadError::Transport {
            plural: entry.plural.clone(),
            message: other.to_string(),
        },
    }
}

pub async fn apiserver_version(client: &Client) -> Result<Info, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async { client.apiserver_version().await.map_err(OpsError::from) },
        OpsError::Timeout,
    )
    .await
}

/// What a server-side check said about a document before it is applied.
#[derive(Debug)]
pub enum ApplyCheck {
    /// The API server accepted the document and returned the object it would store.
    Valid(Arc<DynamicObject>),
    /// Another field manager owns one of the fields this apply would change.
    Conflict { owners: Vec<String> },
}

/// Parse one YAML document and ask the API server to validate it without storing it.
///
/// A local parse cannot see a schema violation, an immutable field, an unknown enum value,
/// or a required field the document omits. Those are the failures that surface *after* an
/// apply has already half-succeeded, which is the worst time to learn about them, so this
/// runs the same server-side apply the real request would run with `dry_run` and strict field
/// validation, and returns the server's verdict instead of writing anything.
pub async fn check_apply_yaml(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    expected_uid: &str,
    yaml: &str,
) -> Result<ApplyCheck, OpsError> {
    let (name, object) = prepare_apply_object(resource, expected_uid, yaml)?;

    let result = with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let mut params = PatchParams::apply(FIELD_MANAGER);
            params.dry_run = true;
            params.field_validation = Some(ValidationDirective::Strict);
            api_for(client, resource, namespace)
                .patch(&name, &params, &Patch::Apply(&object))
                .await
                .map_err(OpsError::from)
        },
        OpsError::Timeout,
    )
    .await;
    match result {
        Ok(checked) => Ok(ApplyCheck::Valid(Arc::new(checked))),
        Err(OpsError::Kube(error)) => match conflict_owners(&error) {
            Some(owners) => Ok(ApplyCheck::Conflict { owners }),
            None => Err(OpsError::Kube(error)),
        },
        Err(error) => Err(error),
    }
}

/// Parse and identity-check one apply document, returning its name and the object to send.
///
/// Shared by the check and the apply so the two can never disagree about what would be sent:
/// if the preflight accepted a document, the apply sends exactly that document.
fn prepare_apply_object(
    resource: &ApiResource,
    expected_uid: &str,
    yaml: &str,
) -> Result<(String, DynamicObject), OpsError> {
    let mut object: DynamicObject = serde_yaml_ng::from_str(yaml)?;
    let name = object.metadata.name.clone().ok_or(OpsError::MissingName)?;

    if let Some(kind) = object
        .types
        .as_ref()
        .map(|types| types.kind.as_str())
        .filter(|kind| !kind.is_empty())
        && kind != resource.kind
    {
        return Err(OpsError::KindMismatch {
            found: kind.to_string(),
            expected: resource.kind.clone(),
        });
    }
    validate_apply_uid(&object, expected_uid)?;
    object.metadata.uid = Some(expected_uid.to_owned());
    Ok((name, object))
}

/// Parse one YAML document and apply it with server-side apply.
pub async fn apply_yaml(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    expected_uid: &str,
    yaml: &str,
) -> Result<ApplyOutcome, OpsError> {
    apply_with_history(client, resource, namespace, expected_uid, yaml, None).await
}

/// Apply one YAML document and record the spec the apply overwrites, so a later apply can be
/// reverted to it (`docs/WRITE-OPS.md` §5).
///
/// This is [`apply_yaml`] plus the two things `k8s-core` cannot see on its own, which is why
/// they are parameters and not constants here.
pub async fn apply_yaml_with_history(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    expected_uid: &str,
    yaml: &str,
    record: &ApplyRecord<'_>,
) -> Result<ApplyOutcome, OpsError> {
    apply_with_history(
        client,
        resource,
        namespace,
        expected_uid,
        yaml,
        Some(record),
    )
    .await
}

/// What a history entry needs that only the caller knows: the cluster the apply ran against, the
/// action that started it, and where `history.json` lives.
#[derive(Clone, Copy, Debug)]
pub struct ApplyRecord<'a> {
    /// Stable cluster ID, as `k8s_core::cluster::ClusterId` renders it.
    pub cluster: &'a str,
    /// The label of the action that started the apply, e.g. `⌘K → Edit YAML`.
    pub applied_by: &'a str,
    /// The `history.json` to write, from `k8s_core::paths::history_file`.
    pub path: &'a Path,
}

async fn apply_with_history(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    expected_uid: &str,
    yaml: &str,
    record: Option<&ApplyRecord<'_>>,
) -> Result<ApplyOutcome, OpsError> {
    let (name, object) = prepare_apply_object(resource, expected_uid, yaml)?;

    // The spec a revert restores is whatever the server holds right now, so it is read before
    // the write rather than reconstructed from what the caller had on screen.
    let overwritten = match record {
        Some(_) => read_overwritten_spec(client, resource, namespace, &name).await,
        None => None,
    };

    let result = with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            api_for(client, resource, namespace)
                .patch(
                    &name,
                    &PatchParams::apply(FIELD_MANAGER),
                    &Patch::Apply(&object),
                )
                .await
                .map_err(OpsError::from)
        },
        OpsError::Timeout,
    )
    .await;
    match result {
        Ok(applied) => {
            // Only an apply the server accepted may leave a history entry. A failed apply
            // changed nothing, so the spec it would have replaced is still the live one, and
            // storing it would hand C8 a revert target that is not "the one before mine".
            if let (Some(record), Some(overwritten)) = (record, overwritten) {
                record_overwritten_spec(record, resource, namespace, &name, overwritten);
            }
            Ok(ApplyOutcome::Applied(Arc::new(applied)))
        }
        Err(OpsError::Kube(error)) => {
            if let Some(owners) = conflict_owners(&error) {
                Ok(ApplyOutcome::Conflict { owners })
            } else if apply_kube_error_is_unknown(&error) {
                Ok(ApplyOutcome::Unknown {
                    reason: error.to_string(),
                })
            } else {
                Err(OpsError::Kube(error))
            }
        }
        Err(OpsError::Timeout) => Ok(ApplyOutcome::Unknown {
            reason: OpsError::Timeout.to_string(),
        }),
        Err(error) => Err(error),
    }
}

/// Read the object an apply is about to overwrite.
///
/// `None` means there is nothing to store — the object may not exist yet (a create) — and the
/// apply goes on regardless: a missing snapshot must never be the reason a write fails.
async fn read_overwritten_spec(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
) -> Option<DynamicObject> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            api_for(client, resource, namespace)
                .get(name)
                .await
                .map_err(OpsError::from)
        },
        OpsError::Timeout,
    )
    .await
    .inspect_err(|error| {
        tracing::debug!(%name, %error, "Could not read the pre-apply spec. No history entry will be stored.");
    })
    .ok()
}

/// Store the pre-apply spec. Both failures are logged and swallowed: the apply already
/// succeeded, and the user is about to be told so.
fn record_overwritten_spec(
    record: &ApplyRecord<'_>,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    overwritten: DynamicObject,
) {
    let target = history::HistoryTarget::new(record.cluster, namespace, &resource.kind, name);
    let spec = match serde_json::to_value(overwritten) {
        Ok(spec) => spec,
        Err(error) => {
            tracing::warn!(%name, %error, "The pre-apply spec could not be serialized. No history entry was stored.");
            return;
        }
    };
    if let Err(error) = history::record_previous_spec(record.path, &target, spec, record.applied_by)
    {
        tracing::warn!(%name, path = %record.path.display(), %error, "The apply history entry could not be written.");
    }
}

fn validate_apply_uid(object: &DynamicObject, expected_uid: &str) -> Result<(), OpsError> {
    if expected_uid.is_empty() {
        return Err(OpsError::MissingUid);
    }
    let Some(found) = object.metadata.uid.as_deref().filter(|uid| !uid.is_empty()) else {
        return Err(OpsError::MissingUid);
    };
    if found != expected_uid {
        return Err(OpsError::UidMismatch {
            expected: expected_uid.to_owned(),
            found: found.to_owned(),
        });
    }
    Ok(())
}

fn apply_kube_error_is_unknown(error: &kube::Error) -> bool {
    match error {
        kube::Error::HyperError(_) | kube::Error::Service(_) | kube::Error::RustlsTls(_) => true,
        kube::Error::Api(status) => matches!(status.code, 408 | 502 | 503 | 504),
        _ => false,
    }
}
/// Extract field managers from a 409 conflict.
fn conflict_owners(error: &kube::Error) -> Option<Vec<String>> {
    let kube::Error::Api(status) = error else {
        return None;
    };
    if !status.is_conflict() {
        return None;
    }

    let mut owners = Vec::new();
    if let Some(details) = &status.details {
        for cause in &details.causes {
            if cause.reason == "FieldManagerConflict"
                && let Some(owner) = quoted_values(&cause.message).into_iter().next()
            {
                push_unique(&mut owners, owner);
            }
        }
    }
    if owners.is_empty() {
        for owner in quoted_values(&status.message) {
            push_unique(&mut owners, owner);
        }
    }
    (!owners.is_empty()).then_some(owners)
}

fn push_unique(owners: &mut Vec<String>, owner: String) {
    if !owners.contains(&owner) {
        owners.push(owner);
    }
}

/// Extract each quoted value from `text`.
fn quoted_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('"') else {
            break;
        };
        values.push(rest[..end].to_string());
        rest = &rest[end + 1..];
    }
    values
}

pub async fn delete_object(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    uid: &str,
) -> Result<(), OpsError> {
    if uid.is_empty() {
        return Err(OpsError::MissingUid);
    }
    let params = DeleteParams::default().preconditions(Preconditions {
        uid: Some(uid.to_owned()),
        ..Preconditions::default()
    });
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            api_for(client, resource, namespace)
                .delete(name, &params)
                .await?;
            Ok::<_, OpsError>(())
        },
        OpsError::Timeout,
    )
    .await
}

fn uid_guarded_patch(
    uid: &str,
    operation: serde_json::Value,
) -> Result<Patch<JsonPatch>, OpsError> {
    if uid.is_empty() {
        return Err(OpsError::MissingUid);
    }
    let patch: JsonPatch = serde_json::from_value(serde_json::json!([
        { "op": "test", "path": "/metadata/uid", "value": uid },
        operation,
    ]))
    .map_err(|error| OpsError::PatchBuild(error.to_string()))?;
    Ok(Patch::Json(patch))
}

/// Patch `spec.replicas` without using the scale subresource.
pub async fn scale(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    uid: &str,
    replicas: i32,
) -> Result<DynamicObject, OpsError> {
    let patch = uid_guarded_patch(
        uid,
        serde_json::json!({ "op": "add", "path": "/spec/replicas", "value": replicas }),
    )?;
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            api_for(client, resource, namespace)
                .patch(name, &PatchParams::default(), &patch)
                .await
                .map_err(OpsError::from)
        },
        OpsError::Timeout,
    )
    .await
}

fn rollout_restart_patch(uid: &str, restarted_at: &str) -> Result<Patch<JsonPatch>, OpsError> {
    uid_guarded_patch(
        uid,
        serde_json::json!({
            "op": "add",
            "path": "/spec/template/metadata/annotations",
            "value": { RESTART_ANNOTATION: restarted_at }
        }),
    )
}

/// Add a timestamp to `spec.template.metadata.annotations` to trigger a rollout.
pub async fn rollout_restart(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    uid: &str,
) -> Result<DynamicObject, OpsError> {
    if uid.is_empty() {
        return Err(OpsError::MissingUid);
    }
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let restarted_at =
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let patch = rollout_restart_patch(uid, &restarted_at)?;
            api_for(client, resource, namespace)
                .patch(name, &PatchParams::default(), &patch)
                .await
                .map_err(OpsError::from)
        },
        OpsError::Timeout,
    )
    .await
}

#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    pub container: Option<String>,
    pub follow: bool,
    pub tail_lines: Option<i64>,
    pub since_seconds: Option<i64>,
    pub timestamps: bool,
    pub previous: bool,
}

fn log_params(options: &LogOptions) -> LogParams {
    LogParams {
        container: options.container.clone(),
        follow: options.follow,
        tail_lines: options.tail_lines,
        since_seconds: options.since_seconds,
        timestamps: options.timestamps,
        previous: options.previous,
        ..LogParams::default()
    }
}

/// A log line or a terminal read error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogItem {
    Line(String),
    Error(String),
}

/// Split a byte stream into lines and normalize CRLF.
#[derive(Debug, Default)]
struct LineSplitter {
    pending: Vec<u8>,
    discarding: bool,
    at_limit_cr: bool,
}

impl LineSplitter {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in chunk {
            if self.discarding {
                if byte == b'\n' {
                    self.discarding = false;
                }
                continue;
            }
            if self.at_limit_cr {
                self.at_limit_cr = false;
                if byte == b'\n' {
                    lines.push(self.take_line());
                    continue;
                }
                lines.push(TRUNCATED_LOG_LINE.to_owned());
                self.pending.clear();
                self.discarding = true;
                continue;
            }
            if byte == b'\n' {
                lines.push(self.take_line());
            } else if self.pending.len() >= MAX_LOG_LINE_BYTES {
                if byte == b'\r' {
                    self.at_limit_cr = true;
                } else {
                    lines.push(TRUNCATED_LOG_LINE.to_owned());
                    self.pending.clear();
                    self.discarding = true;
                }
            } else {
                self.pending.push(byte);
            }
        }
        lines
    }

    fn finish(mut self) -> Option<String> {
        if self.discarding {
            None
        } else if self.at_limit_cr {
            Some(TRUNCATED_LOG_LINE.to_owned())
        } else if self.pending.is_empty() {
            None
        } else {
            Some(self.take_line())
        }
    }

    fn take_line(&mut self) -> String {
        let mut bytes = std::mem::take(&mut self.pending);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Background task that reads and splits the kube log stream.
#[derive(Debug)]
pub struct LogStream {
    lines: mpsc::Receiver<LogItem>,
    task: JoinHandle<()>,
}

impl LogStream {
    /// Return the next log item. `None` means the stream ended.
    pub async fn next(&mut self) -> Option<LogItem> {
        self.lines.recv().await
    }
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read bytes, split lines, and send them to the channel.
async fn pump_lines<R>(reader: R, sender: mpsc::Sender<LogItem>)
where
    R: futures::io::AsyncRead + Send,
{
    let mut reader = Box::pin(reader);
    let mut splitter = LineSplitter::default();
    let mut chunk = vec![0u8; 8192];
    loop {
        match futures::io::AsyncReadExt::read(&mut reader, &mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                for line in splitter.push(&chunk[..read]) {
                    if sender.send(LogItem::Line(line)).await.is_err() {
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = sender.send(LogItem::Error(error.to_string())).await;
                return;
            }
        }
    }
    if let Some(line) = splitter.finish() {
        let _ = sender.send(LogItem::Line(line)).await;
    }
}

/// Open a Pod log stream. A multi-container Pod requires a container name.
pub async fn log_stream(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    options: &LogOptions,
) -> Result<LogStream, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            ensure_pod(resource, "log stream")?;
            if options.container.is_none() {
                let containers = pod_container_names(client, resource, namespace, name).await?;
                if containers.len() > 1 {
                    return Err(OpsError::ContainerSelectionRequired {
                        pod: name.to_string(),
                        containers,
                    });
                }
            }

            let api = api_for(client, resource, namespace);
            let request = Request::new(api.resource_url().to_string())
                .logs(name, &log_params(options))
                .map_err(kube::Error::BuildRequest)?;
            let stream = client.request_stream(request).await?;

            let (sender, lines) = mpsc::channel(LOG_CHANNEL_CAPACITY);
            let task = tokio::spawn(pump_lines(stream, sender));
            Ok(LogStream { lines, task })
        },
        OpsError::Timeout,
    )
    .await
}

fn ensure_pod(resource: &ApiResource, what: &'static str) -> Result<(), OpsError> {
    if resource.group.is_empty() && resource.plural == "pods" {
        return Ok(());
    }
    Err(OpsError::UnsupportedResource {
        what,
        resource: format!("{}/{}", resource.api_version, resource.plural),
    })
}

async fn pod_container_names(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
) -> Result<Vec<String>, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let pod = api_for(client, resource, namespace)
                .get(name)
                .await
                .map_err(OpsError::from)?;
            Ok::<_, OpsError>(container_names(&pod))
        },
        OpsError::Timeout,
    )
    .await
}

fn container_names(pod: &DynamicObject) -> Vec<String> {
    pod.data
        .pointer("/spec/containers")
        .and_then(|containers| containers.as_array())
        .map(|containers| {
            containers
                .iter()
                .filter_map(|container| container.get("name")?.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn events_resource() -> ApiResource {
    ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk("", "v1", "Event"), "events")
}

/// List events for `involvedObject.uid = uid`, sorted by time.
pub async fn list_events_for(
    client: &Client,
    namespace: Option<&str>,
    uid: &str,
) -> Result<Vec<DynamicObject>, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let params = ListParams::default().fields(&format!("involvedObject.uid={uid}"));
            let mut events = api_for(client, &events_resource(), namespace)
                .list(&params)
                .await
                .map_err(OpsError::from)?
                .items;
            events.sort_by_cached_key(event_sort_key);
            Ok::<_, OpsError>(events)
        },
        OpsError::Timeout,
    )
    .await
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EventSortKey {
    timestamp: Option<i128>,
    uid: String,
    name: String,
}

impl Ord for EventSortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        let time_order = match (self.timestamp, other.timestamp) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        time_order
            .then_with(|| self.uid.cmp(&other.uid))
            .then_with(|| self.name.cmp(&other.name))
    }
}

impl PartialOrd for EventSortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn event_sort_key(event: &DynamicObject) -> EventSortKey {
    let timestamp = [
        "/lastTimestamp",
        "/eventTime",
        "/series/lastObservedTime",
        "/firstTimestamp",
    ]
    .iter()
    .find_map(|field| {
        event
            .data
            .pointer(field)
            .and_then(serde_json::Value::as_str)
            .and_then(parse_event_timestamp)
    })
    .or_else(|| {
        event
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|created| created.0.as_nanosecond())
    });
    EventSortKey {
        timestamp,
        uid: event.metadata.uid.clone().unwrap_or_default(),
        name: event.metadata.name.clone().unwrap_or_default(),
    }
}

fn parse_event_timestamp(value: &str) -> Option<i128> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(value).ok()?;
    Some(
        i128::from(timestamp.timestamp()) * 1_000_000_000
            + i128::from(timestamp.timestamp_subsec_nanos()),
    )
}

/// Data for the Describe panel: object, related events, and owner references.
#[derive(Clone, Debug)]
pub struct DescribeData {
    pub object: Arc<DynamicObject>,
    /// Events sorted by `lastTimestamp`.
    pub events: Vec<DynamicObject>,
    /// Owner references as `(kind, name)` pairs.
    pub owners: Vec<(String, String)>,
    /// The owner with `controller: true`, if there is one.
    ///
    /// Carried separately from `owners` because it is the one the UI has to be able
    /// to *follow*, and following needs a uid, not a name. It is also the fact a write
    /// confirmation has to quote, so the two read the same [`controller_of`] rather
    /// than each re-deriving the answer from the raw list.
    pub controller: Option<Controller>,
}

/// Build Describe data from the object, events, and owner references.
///
/// Events are extra context. A failure there leaves the object empty instead of failing
/// the whole request, so a busy or unauthorized event list never hides the resource.
pub async fn describe(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
) -> Result<DescribeData, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            let object = api_for(client, resource, namespace)
                .get(name)
                .await
                .map_err(OpsError::from)?;
            let uid = object.metadata.uid.clone().ok_or(OpsError::MissingUid)?;
            let events =
                describe_events_or_default(list_events_for(client, namespace, &uid).await, name);
            let owners = owner_references(&object);
            let controller = controller_of(&object);
            Ok::<_, OpsError>(DescribeData {
                object: Arc::new(object),
                events,
                owners,
                controller,
            })
        },
        OpsError::Timeout,
    )
    .await
}

/// Build Describe data and reject a result for a different object.
///
/// The name alone is not identity: a Pod or Job recreated under the same name answers
/// with a new UID. Callers that hold the selected UID must use this entry point.
pub async fn describe_expecting(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    expected_uid: &str,
) -> Result<DescribeData, OpsError> {
    let data = describe(client, resource, namespace, name).await?;
    validate_describe_uid(&data.object, expected_uid)?;
    Ok(data)
}

/// Rejects a Describe payload that answers for a different object.
fn validate_describe_uid(object: &DynamicObject, expected_uid: &str) -> Result<(), OpsError> {
    if expected_uid.is_empty() {
        return Ok(());
    }
    let found = object.metadata.uid.clone().unwrap_or_default();
    if found == expected_uid {
        return Ok(());
    }
    Err(OpsError::UidMismatch {
        expected: expected_uid.to_owned(),
        found,
    })
}

/// Keeps the object readable when the optional event list fails.
fn describe_events_or_default(
    result: Result<Vec<DynamicObject>, OpsError>,
    name: &str,
) -> Vec<DynamicObject> {
    match result {
        Ok(events) => events,
        Err(error) => {
            eprintln!("k8s-gpui: describe events unavailable for {name}: {error}");
            Vec::new()
        }
    }
}

fn owner_references(object: &DynamicObject) -> Vec<(String, String)> {
    object
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|owner| (owner.kind.clone(), owner.name.clone()))
        .collect()
}

/// An object that owns this one, and the part of it that makes it the owner.
///
/// `WRITE-OPS.md` §2.2 and `UI-REDESIGN.md` L3 both reduce "will this be recreated
/// if I change it?" to a single fact: does `metadata.ownerReferences` hold an owner
/// with `controller: true`. This is that fact, and it is the **only** place it is
/// computed — the Related block reads it, and the write-side confirmation copy that
/// `WRITE-OPS.md` §2.2 requires reuses this rather than writing a second judgement
/// that could disagree with the first.
///
/// A missing `controller` key and an explicit `false` are the same answer, and both
/// are different from an owner reference with no such owner at all: a list entry is a
/// claim about lineage, so an owner present without the flag is something a reader
/// needs to see and something a confirmation must not treat as a controller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Controller {
    pub kind: String,
    pub name: String,
    /// The owner's uid. This is what makes the owner followable: the query language's
    /// `owner=<uid>` clause matches on it, so the Related block can hand a reader the
    /// rows behind this relationship instead of a name to retype.
    pub uid: String,
}

/// The controller of `object`, if it has one.
///
/// Returns `None` for an object with no owner references, and for one whose owners all
/// lack `controller: true`. Callers that need to tell those two cases apart — the
/// Related block does — must read `owner_references` as well; this function answers
/// only the question the spec asks.
pub fn controller_of(object: &DynamicObject) -> Option<Controller> {
    object
        .metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|owner| owner.controller.unwrap_or(false))
        .map(|owner| Controller {
            kind: owner.kind.clone(),
            name: owner.name.clone(),
            uid: owner.uid.clone(),
        })
}

#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    /// Target container. `None` requires a Pod with one container.
    pub container: Option<String>,
    /// Allocate a TTY and enable stdin. The container merges stderr into stdout.
    pub tty: bool,
    /// Command to run in the container.
    pub command: Vec<String>,
    /// Extra environment variables. The command uses an `env` prefix when this list is not empty.
    pub env: Vec<(String, String)>,
}

/// Exec exit status. A non-zero command exit is returned by [`ExecSession::status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecStatus {
    pub code: i32,
    pub reason: Option<String>,
    pub message: Option<String>,
}

/// An established exec session. Stdin and resize require a TTY.
pub struct ExecSession {
    stdin: Option<Box<dyn AsyncWrite + Send + Unpin>>,
    stdout: Option<Box<dyn AsyncRead + Send + Unpin>>,
    stderr: Option<Box<dyn AsyncRead + Send + Unpin>>,
    resize: Option<futures_mpsc::Sender<TerminalSize>>,
    status: Option<Pin<Box<dyn Future<Output = Option<K8sStatus>> + Send>>>,
    last_status: Option<ExecStatus>,
    process: AttachedProcess,
}

impl fmt::Debug for ExecSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecSession")
            .field("has_stdin", &self.stdin.is_some())
            .field("has_stderr", &self.stderr.is_some())
            .finish_non_exhaustive()
    }
}

impl ExecSession {
    /// Stdin writer. It is `None` without a TTY.
    pub fn stdin(&mut self) -> Option<&mut (dyn AsyncWrite + Send + Unpin + 'static)> {
        self.stdin.as_deref_mut()
    }

    pub fn stdout(&mut self) -> Option<&mut (dyn AsyncRead + Send + Unpin + 'static)> {
        self.stdout.as_deref_mut()
    }

    /// Stderr reader. It is `None` for a TTY session.
    pub fn stderr(&mut self) -> Option<&mut (dyn AsyncRead + Send + Unpin + 'static)> {
        self.stderr.as_deref_mut()
    }

    pub fn take_stdin(&mut self) -> Option<Box<dyn AsyncWrite + Send + Unpin>> {
        self.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<Box<dyn AsyncRead + Send + Unpin>> {
        self.stderr.take()
    }

    /// Send a terminal size. A full channel drops the update.
    pub fn resize(&mut self, size: TerminalSize) -> Result<(), OpsError> {
        let Some(sender) = self.resize.as_mut() else {
            return Err(OpsError::ResizeWithoutTty);
        };
        match sender.try_send(size) {
            Ok(()) => Ok(()),
            Err(error) if error.is_disconnected() => Err(OpsError::ExecDisconnected),
            Err(_full) => Ok(()),
        }
    }

    /// Wait for the command and cache its exit status.
    pub async fn status(&mut self) -> Result<ExecStatus, OpsError> {
        if let Some(status) = &self.last_status {
            return Ok(status.clone());
        }
        let Some(status) = self.status.take() else {
            return Err(OpsError::ExecDisconnected);
        };
        let Some(status) = status.await else {
            return Err(OpsError::ExecDisconnected);
        };
        let status = exec_status(&status);
        self.last_status = Some(status.clone());
        Ok(status)
    }

    /// Cancel the session.
    pub fn cancel(self) {
        self.process.abort();
    }
}

/// Build exec request parameters. TTY sessions do not open stderr.
fn attach_params(options: &ExecOptions) -> AttachParams {
    AttachParams {
        container: options.container.clone(),
        stdin: options.tty,
        stdout: true,
        stderr: !options.tty,
        tty: options.tty,
        max_stdin_buf_size: Some(16 * 1024),
        max_stdout_buf_size: Some(64 * 1024),
        max_stderr_buf_size: Some(64 * 1024),
    }
}

/// Prefix the command with `env` when extra variables are set.
fn exec_command(options: &ExecOptions) -> Vec<String> {
    if options.env.is_empty() {
        return options.command.clone();
    }
    let mut command = Vec::with_capacity(options.command.len() + options.env.len() + 1);
    command.push("env".to_string());
    command.extend(
        options
            .env
            .iter()
            .map(|(key, value)| format!("{key}={value}")),
    );
    command.extend(options.command.iter().cloned());
    command
}

/// Parse the API server exec exit status.
fn exec_status(status: &K8sStatus) -> ExecStatus {
    let code = status
        .details
        .as_ref()
        .and_then(|details| details.causes.as_deref())
        .and_then(|causes| {
            causes
                .iter()
                .find(|cause| cause.reason.as_deref() == Some("ExitCode"))
                .and_then(|cause| cause.message.as_deref())
                .and_then(|message| message.trim().parse::<i32>().ok())
        })
        .unwrap_or(if status.status.as_deref() == Some("Success") {
            0
        } else {
            1
        });
    ExecStatus {
        code,
        reason: status.reason.clone().filter(|reason| !reason.is_empty()),
        message: status.message.clone().filter(|message| !message.is_empty()),
    }
}

/// Run a command in a Pod. TTY sessions merge stderr into stdout.
pub async fn exec(
    client: &Client,
    resource: &ApiResource,
    namespace: Option<&str>,
    name: &str,
    options: &ExecOptions,
) -> Result<ExecSession, OpsError> {
    with_read_timeout(
        NON_WATCH_READ_TIMEOUT,
        async {
            ensure_pod(resource, "exec")?;
            let namespace = namespace.ok_or(OpsError::NamespaceRequired { what: "exec" })?;
            if options.command.is_empty() {
                return Err(OpsError::MissingCommand { what: "exec" });
            }
            // Validate the container before opening the websocket.
            let containers = pod_container_names(client, resource, Some(namespace), name).await?;
            match &options.container {
                Some(container) if !containers.iter().any(|known| known == container) => {
                    return Err(OpsError::ContainerNotFound {
                        pod: name.to_string(),
                        container: container.clone(),
                        containers,
                    });
                }
                Some(_) => {}
                None if containers.len() > 1 => {
                    return Err(OpsError::ContainerSelectionRequired {
                        pod: name.to_string(),
                        containers,
                    });
                }
                None => {}
            }

            let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
            let mut process = pods
                .exec(name, exec_command(options), &attach_params(options))
                .await?;
            let stdin = process
                .stdin()
                .map(|stream| Box::new(stream) as Box<dyn AsyncWrite + Send + Unpin>);
            let stdout = process
                .stdout()
                .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Send + Unpin>);
            let stderr = process
                .stderr()
                .map(|stream| Box::new(stream) as Box<dyn AsyncRead + Send + Unpin>);
            let resize = process.terminal_size();
            let status = process.take_status().map(|status| {
                Box::pin(status) as Pin<Box<dyn Future<Output = Option<K8sStatus>> + Send>>
            });

            Ok(ExecSession {
                stdin,
                stdout,
                stderr,
                resize,
                status,
                last_status: None,
                process,
            })
        },
        OpsError::Timeout,
    )
    .await
}

/// One established port-forward connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortForward {
    pub remote_port: u16,
    pub local_port: u16,
}

/// Runtime error for a port-forward stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortForwardError {
    pub port: u16,
    pub reason: String,
}

/// Port-forward session with one local listener per remote port.
pub struct PortForwardSession {
    forwards: Vec<PortForward>,
    errors: mpsc::UnboundedReceiver<PortForwardError>,
    tasks: Vec<JoinHandle<()>>,
    forwarder: Portforwarder,
}

impl PortForwardSession {
    /// Start port forwarding. The system assigns each local port.
    pub async fn start(
        client: &Client,
        resource: &ApiResource,
        namespace: Option<&str>,
        name: &str,
        ports: Vec<u16>,
    ) -> Result<Self, OpsError> {
        Self::start_with_local_ports(client, resource, namespace, name, ports, &[]).await
    }

    /// Start port forwarding with an optional requested local port per remote port.
    /// `local_ports` is read by position: entry `i` asks for the local port of `ports[i]`, and
    /// a missing entry or `None` leaves that port to the system.
    ///
    /// A requested port that is taken fails the forward rather than moving to a free one. The
    /// reader asked for an address, and whatever is already pointed at that address is pointed at
    /// nothing after a substitution — which is the same refusal `kubectl port-forward` gives, and
    /// the one the port-forward form already gives before it submits, so one collision has one
    /// answer instead of two that depend on who got there first.
    pub async fn start_with_local_ports(
        client: &Client,
        resource: &ApiResource,
        namespace: Option<&str>,
        name: &str,
        ports: Vec<u16>,
        local_ports: &[Option<u16>],
    ) -> Result<Self, OpsError> {
        with_read_timeout(
            NON_WATCH_READ_TIMEOUT,
            async {
                ensure_pod(resource, "port forward")?;
                let namespace = namespace.ok_or(OpsError::NamespaceRequired {
                    what: "port forward",
                })?;

                let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
                let forwarder = pods.portforward(name, &ports).await?;
                let (error_sender, errors) = mpsc::unbounded_channel();
                let mut session = Self {
                    forwards: Vec::with_capacity(ports.len()),
                    errors,
                    tasks: Vec::with_capacity(ports.len() * 2),
                    forwarder,
                };

                for (index, port) in ports.into_iter().enumerate() {
                    let Some(stream) = session.forwarder.take_stream(port) else {
                        return Err(OpsError::MissingForwardStream { port });
                    };
                    let requested = local_ports.get(index).copied().flatten();
                    let listener = bind_local_listener(requested, port).await?;
                    let local_port = match listener.local_addr() {
                        Ok(address) => address.port(),
                        // The listener is open; there is simply no port to report, so the
                        // failure cannot name one it never bound.
                        Err(source) => {
                            return Err(OpsError::AssignLocalPort {
                                remote: port,
                                source,
                            });
                        }
                    };
                    session.forwards.push(PortForward {
                        remote_port: port,
                        local_port,
                    });

                    if let Some(error) = session.forwarder.take_error(port) {
                        let error_sender = error_sender.clone();
                        session.tasks.push(tokio::spawn(async move {
                            if let Some(reason) = error.await {
                                let _ = error_sender.send(PortForwardError { port, reason });
                            }
                        }));
                    }
                    session
                        .tasks
                        .push(tokio::spawn(forward_port(listener, stream)));
                }

                Ok(session)
            },
            OpsError::Timeout,
        )
        .await
    }

    /// Local listening ports.
    pub fn local_ports(&self) -> Vec<u16> {
        self.forwards
            .iter()
            .map(|forward| forward.local_port)
            .collect()
    }

    /// Return the next runtime error. `None` means no more errors.
    pub async fn next_error(&mut self) -> Option<PortForwardError> {
        self.errors.recv().await
    }

    /// Stop port forwarding.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.forwarder.abort();
        abort_all(&self.tasks);
    }
}

impl Drop for PortForwardSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl fmt::Debug for PortForwardSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortForwardSession")
            .field("forwards", &self.forwards)
            .finish_non_exhaustive()
    }
}

fn abort_all(tasks: &[JoinHandle<()>]) {
    for task in tasks {
        task.abort();
    }
}

/// Bind the local listener for one forward.
///
/// A requested port that is taken is the reader's answer, not a detail to work around: the
/// forward refuses it and names the port, which is the same sentence the port-forward form
/// shows before it submits.
async fn bind_local_listener(
    requested: Option<u16>,
    remote_port: u16,
) -> Result<TcpListener, OpsError> {
    let Some(port) = requested else {
        return bind_free_local_port(remote_port).await;
    };
    TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .map_err(|source| OpsError::BindLocalPort { port, source })
}

/// Let the system pick a free local port.
async fn bind_free_local_port(remote_port: u16) -> Result<TcpListener, OpsError> {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|source| OpsError::AssignLocalPort {
            remote: remote_port,
            source,
        })
}

/// Serve one local connection at a time for each forwarded port.
async fn forward_port<Remote>(listener: TcpListener, mut remote: Remote)
where
    Remote: AsyncRead + AsyncWrite + Send + Unpin,
{
    loop {
        let Ok((mut local, _peer)) = listener.accept().await else {
            return;
        };
        let _ = tokio::io::copy_bidirectional(&mut local, &mut remote).await;
    }
}

fn api_for(client: &Client, resource: &ApiResource, namespace: Option<&str>) -> Api<DynamicObject> {
    match namespace {
        Some(namespace) => Api::namespaced_with(client.clone(), namespace, resource),
        None => Api::all_with(client.clone(), resource),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::Config;
    use kube::core::GroupVersionKind;
    use serde_json::json;

    /// The controller judgement, over every shape an owner list can take.
    ///
    /// This is the one function `WRITE-OPS.md` §2.2 tells a write confirmation to
    /// reuse, so the table covers the shapes a real apiserver returns: no owners at
    /// all, an owner with no `controller` key, an explicit `false`, a `true` sitting
    /// beside a non-controller, and two `true` (which Kubernetes forbids, but a bad
    /// client could send, and where "the first" is the only answer that does not
    /// depend on iteration order).
    ///
    /// The object is built from JSON on purpose: it is the shape the API server sends,
    /// so a field this test forgets is a field the real code path also forgets.
    #[test]
    fn the_controller_is_the_owner_that_says_so() {
        fn object_with(owners: serde_json::Value) -> DynamicObject {
            serde_json::from_value(json!({
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {
                    "name": "api",
                    "uid": "uid-api",
                    "ownerReferences": owners,
                },
            }))
            .expect("a deployment")
        }

        fn owner(kind: &str, name: &str, uid: &str, controller: Option<bool>) -> serde_json::Value {
            let mut owner = json!({
                "apiVersion": "apps/v1",
                "kind": kind,
                "name": name,
                "uid": uid,
            });
            if let Some(controller) = controller {
                owner["controller"] = json!(controller);
            }
            owner
        }

        // No owners at all: not controlled, and `owners` is empty too, so a caller can
        // tell this apart from an owner that exists but is not the controller.
        let bare = object_with(json!([]));
        assert_eq!(controller_of(&bare), None);
        assert!(owner_references(&bare).is_empty());

        // An owner with no `controller` key: present lineage, no controller.
        let unflagged = object_with(json!([owner("ReplicaSet", "web-rs", "uid-rs", None)]));
        assert_eq!(controller_of(&unflagged), None);
        assert_eq!(owner_references(&unflagged).len(), 1);

        // Explicitly not a controller: the same answer, and a different one from having
        // no owners at all.
        let not_controller = object_with(json!([owner("Node", "node-a", "uid-node", Some(false))]));
        assert_eq!(controller_of(&not_controller), None);

        // The real case: one controller among non-controllers. The uid has to survive,
        // because the Related block follows an owner by uid and not by name.
        let mixed = object_with(json!([
            owner("Node", "node-a", "uid-node", Some(false)),
            owner("ReplicaSet", "web-rs", "uid-rs", Some(true)),
        ]));
        assert_eq!(
            controller_of(&mixed),
            Some(Controller {
                kind: "ReplicaSet".to_owned(),
                name: "web-rs".to_owned(),
                uid: "uid-rs".to_owned(),
            })
        );
        // Both owners are still listed; the judgement narrows, it does not replace.
        assert_eq!(owner_references(&mixed).len(), 2);

        // Two controllers: the first, deterministically.
        let two = object_with(json!([
            owner("ReplicaSet", "web-rs", "uid-rs", Some(true)),
            owner("ReplicaSet", "web-canary", "uid-canary", Some(true)),
        ]));
        assert_eq!(
            controller_of(&two).map(|c| c.name),
            Some("web-rs".to_owned())
        );
    }

    fn pod_resource() -> ApiResource {
        ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk("", "v1", "Pod"), "pods")
    }

    fn configmap_resource() -> ApiResource {
        ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("", "v1", "ConfigMap"),
            "configmaps",
        )
    }

    async fn test_client() -> Client {
        let config = Config::new("http://127.0.0.1:1".parse().expect("valid uri"));
        Client::try_from(config).expect("build client")
    }

    #[test]
    fn describe_rejects_a_payload_for_another_object() {
        let object: DynamicObject = serde_yaml_ng::from_str(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-new\n",
        )
        .expect("parse object");
        let mismatch = validate_describe_uid(&object, "uid-old")
            .expect_err("a recreated object must be rejected");
        assert!(matches!(mismatch, OpsError::UidMismatch { .. }));
        assert!(validate_describe_uid(&object, "uid-new").is_ok());
        assert!(
            validate_describe_uid(&object, "").is_ok(),
            "callers without a UID keep the name-only behaviour"
        );
        let without_uid: DynamicObject =
            serde_yaml_ng::from_str("apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n")
                .expect("parse object");
        assert!(validate_describe_uid(&without_uid, "uid-old").is_err());
    }

    #[tokio::test]
    async fn describe_expecting_fails_when_the_object_is_unreachable() {
        let client = test_client().await;
        let error = describe_expecting(&client, &pod_resource(), Some("default"), "web", "uid-web")
            .await
            .expect_err("an unreachable cluster must fail");
        assert!(matches!(error, OpsError::Kube(_)), "unexpected error");
    }

    /// The preflight and the apply must agree about what would be sent, or a document the
    /// server accepted can still fail on the real request.
    #[test]
    fn the_preflight_and_the_apply_send_the_same_document() {
        let resource = pod_resource();
        // The document carries the UID it was read at, which is what makes the identity
        // check meaningful: a buffer that went stale fails here rather than on the real write.
        let yaml = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-web\nspec:\n  containers: []\n";

        let (check_name, check_object) =
            prepare_apply_object(&resource, "uid-web", yaml).expect("preflight parses");
        let (apply_name, apply_object) =
            prepare_apply_object(&resource, "uid-web", yaml).expect("apply parses");

        assert_eq!(check_name, "web");
        assert_eq!(check_name, apply_name);
        assert_eq!(
            serde_json::to_value(&check_object).expect("serialize"),
            serde_json::to_value(&apply_object).expect("serialize"),
            "the preflight validates exactly the document the apply would send"
        );
        // The UID is stamped from the live object, so a stale buffer cannot overwrite a
        // resource that was recreated under a new UID.
        assert_eq!(check_object.metadata.uid.as_deref(), Some("uid-web"));
    }

    #[test]
    fn the_preflight_rejects_a_document_the_apply_would_reject() {
        let resource = pod_resource();
        // A stale buffer must fail the check, not pass it and fail on the real write.
        let stale = prepare_apply_object(
            &resource,
            "uid-new",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-old\n",
        );
        assert!(
            stale.is_err(),
            "a UID mismatch fails before any request is sent"
        );
        // A document for a different kind fails the same way.
        let wrong_kind = prepare_apply_object(
            &resource,
            "uid-web",
            "apiVersion: v1\nkind: Service\nmetadata:\n  name: web\n",
        );
        assert!(matches!(wrong_kind, Err(OpsError::KindMismatch { .. })));
        // A nameless document is rejected too.
        let nameless = prepare_apply_object(
            &resource,
            "uid-web",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  uid: uid-web\n",
        );
        assert!(matches!(nameless, Err(OpsError::MissingName)));
    }

    #[test]
    fn scale_patch_tests_uid_before_changing_replicas() {
        let patch = uid_guarded_patch(
            "uid-old",
            json!({ "op": "add", "path": "/spec/replicas", "value": 4 }),
        )
        .expect("build UID patch");
        let Patch::Json(patch) = patch else {
            panic!("scale must use JSON patch");
        };
        let mut object = json!({
            "metadata": { "uid": "uid-new" },
            "spec": { "replicas": 1 }
        });
        assert!(json_patch::patch(&mut object, &patch).is_err());
        assert_eq!(object.pointer("/spec/replicas"), Some(&json!(1)));
    }

    #[test]
    fn restart_patch_tests_uid_before_setting_annotation() {
        let patch = rollout_restart_patch("uid-restart", "2026-09-25T00:00:00Z")
            .expect("build restart patch");
        let Patch::Json(patch) = patch else {
            panic!("restart must use JSON patch");
        };
        assert_eq!(
            serde_json::to_value(&patch).expect("serialize restart patch"),
            json!([
                { "op": "test", "path": "/metadata/uid", "value": "uid-restart" },
                {
                    "op": "add",
                    "path": "/spec/template/metadata/annotations",
                    "value": { RESTART_ANNOTATION: "2026-09-25T00:00:00Z" }
                }
            ])
        );

        let mut matching = json!({
            "metadata": { "uid": "uid-restart" },
            "spec": { "template": { "metadata": {} } }
        });
        json_patch::patch(&mut matching, &patch).expect("matching UID patch");
        assert_eq!(
            matching
                .pointer("/spec/template/metadata/annotations")
                .and_then(|annotations| annotations.get(RESTART_ANNOTATION)),
            Some(&json!("2026-09-25T00:00:00Z"))
        );

        let mut changed = json!({
            "metadata": { "uid": "uid-new" },
            "spec": { "template": { "metadata": {} } }
        });
        assert!(json_patch::patch(&mut changed, &patch).is_err());
        assert!(
            changed
                .pointer("/spec/template/metadata/annotations")
                .is_none()
        );
    }

    #[tokio::test]
    async fn apply_transport_failure_is_unknown() {
        let client = test_client().await;
        let result = apply_yaml(
            &client,
            &pod_resource(),
            Some("default"),
            "uid-web",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-web\n",
        )
        .await
        .expect("transport failure is classified");
        assert!(matches!(result, ApplyOutcome::Unknown { .. }));
    }

    #[tokio::test]
    async fn apply_rejects_invalid_yaml() {
        let client = test_client().await;
        let error = apply_yaml(
            &client,
            &pod_resource(),
            Some("default"),
            "uid-web",
            "metadata: [oops",
        )
        .await
        .expect_err("invalid yaml must fail");
        assert!(matches!(error, OpsError::Yaml(_)));
    }

    #[tokio::test]
    async fn apply_requires_name() {
        let client = test_client().await;
        let yaml = "apiVersion: v1\nkind: Pod\nmetadata:\n  namespace: default\n";
        let error = apply_yaml(&client, &pod_resource(), Some("default"), "uid-web", yaml)
            .await
            .expect_err("missing name must fail");
        assert!(matches!(error, OpsError::MissingName));
    }

    #[tokio::test]
    async fn apply_rejects_kind_mismatch() {
        let client = test_client().await;
        let yaml = "apiVersion: v1\nkind: Service\nmetadata:\n  name: web\n";
        let error = apply_yaml(&client, &pod_resource(), Some("default"), "uid-web", yaml)
            .await
            .expect_err("kind mismatch must fail");
        match error {
            OpsError::KindMismatch { found, expected } => {
                assert_eq!(found, "Service");
                assert_eq!(expected, "Pod");
                assert_eq!(
                    OpsError::KindMismatch {
                        found: found.clone(),
                        expected: expected.clone(),
                    }
                    .to_string(),
                    "YAML kind Service does not match the target resource Pod. Use YAML for Pod."
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_rejects_missing_or_mismatched_uid() {
        let client = test_client().await;
        let missing = apply_yaml(
            &client,
            &pod_resource(),
            Some("default"),
            "uid-web",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n",
        )
        .await
        .expect_err("missing UID must fail");
        assert!(matches!(missing, OpsError::MissingUid));

        let mismatch = apply_yaml(
            &client,
            &pod_resource(),
            Some("default"),
            "uid-web",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-new\n",
        )
        .await
        .expect_err("mismatched UID must fail");
        assert!(matches!(
            mismatch,
            OpsError::UidMismatch { expected, found }
                if expected == "uid-web" && found == "uid-new"
        ));

        let missing_expected = apply_yaml(
            &client,
            &pod_resource(),
            Some("default"),
            "",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-web\n",
        )
        .await
        .expect_err("missing expected UID must fail");
        assert!(matches!(missing_expected, OpsError::MissingUid));
    }

    /// A fake API server that answers `GET` with `object` and answers the apply with
    /// `patch_status`, so the history hook can be exercised without a cluster.
    async fn fake_api(object: serde_json::Value, patch_status: u16) -> Client {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind fake api");
        let address = listener.local_addr().expect("fake api address");
        let live = object.to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let live = live.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 512];
                    let head = loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => {
                                request.extend_from_slice(&chunk[..read]);
                                if let Some(head) =
                                    request.windows(4).position(|end| end == b"\r\n\r\n")
                                {
                                    break head + 4;
                                }
                            }
                        }
                    };
                    let length = content_length(&request[..head]);
                    while request.len() < head + length {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&chunk[..read]),
                        }
                    }
                    let (status, body) = if request.starts_with(b"GET") {
                        (200, live)
                    } else {
                        (
                            patch_status,
                            String::from(
                                r#"{"kind":"Status","apiVersion":"v1","status":"Failure","code":500,"message":"injected apply failure"}"#,
                            ),
                        )
                    };
                    let reason = if status == 200 { "OK" } else { "Failure" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });
        let config = Config::new(format!("http://{address}").parse().expect("valid uri"));
        Client::try_from(config).expect("build fake client")
    }

    fn content_length(head: &[u8]) -> usize {
        std::str::from_utf8(head)
            .ok()
            .and_then(|head| {
                head.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().ok())?
                })
            })
            .unwrap_or(0)
    }

    /// The stored spec is the one the apply overwrote, and only an apply the server accepted may
    /// store it: a failed apply leaves the live spec in place, so a revert to a stored copy of it
    /// would be a revert to itself.
    #[tokio::test]
    async fn only_an_accepted_apply_stores_the_spec_it_overwrote() {
        let live = |image: &str| {
            json!({
                "apiVersion": "v1",
                "kind": "Pod",
                "metadata": { "name": "web", "namespace": "default", "uid": "uid-web" },
                "spec": { "containers": [ { "name": "app", "image": image } ] }
            })
        };
        let yaml = "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\n  uid: uid-web\nspec:\n  containers: [ { name: app, image: app:2 } ]\n";
        let directory =
            std::env::temp_dir().join(format!("k8s-gpui-apply-history-{}", std::process::id()));
        let path = directory.join("history.json");
        let record = ApplyRecord {
            cluster: "000000000000000a",
            applied_by: "⌘⇧↵ Apply",
            path: &path,
        };

        let accepted = fake_api(live("app:1"), 200).await;
        let outcome = apply_yaml_with_history(
            &accepted,
            &pod_resource(),
            Some("default"),
            "uid-web",
            yaml,
            &record,
        )
        .await
        .expect("apply");
        assert!(matches!(outcome, ApplyOutcome::Applied(_)), "{outcome:?}");

        // The rejected apply is served a different live spec, so storing it would be visible.
        let rejected = fake_api(live("app:9"), 500).await;
        assert!(
            apply_yaml_with_history(
                &rejected,
                &pod_resource(),
                Some("default"),
                "uid-web",
                yaml,
                &record,
            )
            .await
            .is_err(),
            "a rejected apply must not report success"
        );

        let (store, error) = history::HistoryStore::load(&path);
        assert!(error.is_none(), "{error:?}");
        let entry = store
            .previous(&history::HistoryTarget::new(
                "000000000000000a",
                Some("default"),
                "Pod",
                "web",
            ))
            .expect("the accepted apply stored the live spec");
        assert_eq!(
            entry.spec,
            live("app:1"),
            "the accepted apply stored the spec it overwrote, the rejected one stored nothing"
        );
        assert_eq!(entry.applied_by, "⌘⇧↵ Apply");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn log_stream_rejects_non_pod_resource() {
        let client = test_client().await;
        let deployments = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("apps", "v1", "Deployment"),
            "deployments",
        );
        let error = log_stream(
            &client,
            &deployments,
            Some("default"),
            "web",
            &LogOptions::default(),
        )
        .await
        .expect_err("only pods have logs");
        match error {
            OpsError::UnsupportedResource { resource, .. } => {
                assert_eq!(resource, "apps/v1/deployments");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_rejects_non_pod_resource() {
        let client = test_client().await;
        let deployments = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("apps", "v1", "Deployment"),
            "deployments",
        );
        let error = exec(
            &client,
            &deployments,
            Some("default"),
            "web",
            &ExecOptions {
                command: vec!["sh".to_string()],
                ..ExecOptions::default()
            },
        )
        .await
        .expect_err("only pods can exec");
        match error {
            OpsError::UnsupportedResource { resource, .. } => {
                assert_eq!(resource, "apps/v1/deployments");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_requires_namespace_and_command() {
        let client = test_client().await;
        let options = ExecOptions {
            command: vec!["sh".to_string()],
            ..ExecOptions::default()
        };
        let error = exec(&client, &pod_resource(), None, "web", &options)
            .await
            .expect_err("pods need a namespace");
        assert!(matches!(error, OpsError::NamespaceRequired { .. }));

        let error = exec(
            &client,
            &pod_resource(),
            Some("default"),
            "web",
            &ExecOptions::default(),
        )
        .await
        .expect_err("empty command must fail before connecting");
        assert!(matches!(error, OpsError::MissingCommand { .. }));
    }

    #[tokio::test]
    async fn a_requested_local_port_is_the_port_the_forward_listens_on() {
        let reserved =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("find a free local port");
        let port = reserved.local_addr().expect("local address").port();
        // Release the port first: holding it here would make the request a taken port.
        drop(reserved);
        let listener = bind_local_listener(Some(port), 8080)
            .await
            .expect("a free requested port binds as asked");
        assert_eq!(
            listener.local_addr().expect("local address").port(),
            port,
            "the user asked for this port, so the forward must not move"
        );
        drop(listener);
    }

    /// A taken local port ends the forward instead of moving it.
    ///
    /// It used to bind a free port instead and report the substitution, which gave one
    /// collision two answers: the port-forward form refuses the port before it submits, and
    /// the runtime accepted it and answered on a different one. Whatever was pointed at the
    /// port the reader typed was pointed at nothing either way, and the refusal is what
    /// `kubectl port-forward` does.
    #[tokio::test]
    async fn a_taken_local_port_refuses_the_forward_and_names_the_port() {
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupy a local port");
        let port = occupied.local_addr().expect("local address").port();
        let error = bind_local_listener(Some(port), 8080)
            .await
            .expect_err("a taken port must not move the forward to another one");
        let sentence = error.to_string();
        match &error {
            OpsError::BindLocalPort { port: named, .. } => assert_eq!(
                *named, port,
                "the failure carries the port the reader typed, not the remote one"
            ),
            other => panic!("expected BindLocalPort, got {other:?}"),
        }
        assert!(
            sentence.contains(&port.to_string()),
            "the sentence names the port: {sentence}"
        );
        assert!(
            sentence.contains("Choose another local port"),
            "the refusal says what to do: {sentence}"
        );
    }

    #[tokio::test]
    async fn no_requested_local_port_leaves_the_choice_to_the_system() {
        let listener = bind_local_listener(None, 9090)
            .await
            .expect("an automatic port always binds");
        assert!(listener.local_addr().expect("local address").port() > 0);
    }

    #[tokio::test]
    async fn port_forward_rejects_non_pod_resource() {
        let client = test_client().await;
        let deployments = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("apps", "v1", "Deployment"),
            "deployments",
        );
        let error =
            PortForwardSession::start(&client, &deployments, Some("default"), "web", vec![80])
                .await
                .expect_err("only pods can port-forward");
        match error {
            OpsError::UnsupportedResource { resource, .. } => {
                assert_eq!(resource, "apps/v1/deployments");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn port_forward_rejects_empty_ports() {
        let client = test_client().await;
        let error =
            PortForwardSession::start(&client, &pod_resource(), Some("default"), "web", Vec::new())
                .await
                .expect_err("empty ports must fail before connecting");
        assert!(matches!(error, OpsError::Kube(_)));
        assert!(
            error.to_string().contains("ports cannot be empty"),
            "error must be readable: {error}"
        );
    }

    #[tokio::test]
    async fn port_forward_requires_namespace() {
        let client = test_client().await;
        let error = PortForwardSession::start(&client, &pod_resource(), None, "web", vec![80])
            .await
            .expect_err("pods need a namespace");
        assert!(matches!(error, OpsError::NamespaceRequired { .. }));
    }

    #[tokio::test]
    async fn port_forward_unreachable_cluster_is_readable() {
        let client = test_client().await;
        let error =
            PortForwardSession::start(&client, &pod_resource(), Some("default"), "web", vec![80])
                .await
                .expect_err("unreachable cluster must fail");
        match error {
            OpsError::Kube(error) => {
                let message = error.to_string();
                assert!(!message.is_empty(), "error must have a readable message");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    struct DropFlag(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A futures reader that never becomes ready.
    struct PendingReader {
        waker: Option<std::task::Waker>,
    }

    impl futures::io::AsyncRead for PendingReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            _buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.waker = Some(cx.waker().clone());
            std::task::Poll::Pending
        }
    }

    /// A log stream that never ends. DropFlag records task cancellation.
    async fn pending_stream() -> (LogStream, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&dropped);
        let (sender, lines) = mpsc::channel(4);
        let task = tokio::spawn(async move {
            let _guard = DropFlag(flag);
            pump_lines(PendingReader { waker: None }, sender).await;
        });
        // Start the background task before testing cancellation.
        // Tokio does not drop a future that was never polled when the task is aborted.
        tokio::task::yield_now().await;
        (LogStream { lines, task }, dropped)
    }

    async fn assert_aborted(dropped: &std::sync::atomic::AtomicBool) {
        for _ in 0..50 {
            if dropped.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        panic!("background task was not aborted");
    }

    #[tokio::test]
    async fn drop_aborts_the_background_task() {
        let (stream, dropped) = pending_stream().await;
        drop(stream);
        assert_aborted(&dropped).await;
    }

    /// A futures reader that returns preset chunks one at a time.
    struct ChunkReader {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl futures::io::AsyncRead for ChunkReader {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut [u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let Some(chunk) = self.chunks.pop_front() else {
                return std::task::Poll::Ready(Ok(0));
            };
            let read = chunk.len().min(buf.len());
            buf[..read].copy_from_slice(&chunk[..read]);
            if read < chunk.len() {
                self.chunks.push_front(chunk[read..].to_vec());
            }
            std::task::Poll::Ready(Ok(read))
        }
    }

    #[tokio::test]
    async fn pump_delivers_partial_chunks_and_final_line() {
        let reader = ChunkReader {
            chunks: [b"hello\nwor".to_vec(), b"ld".to_vec()].into(),
        };
        let (sender, mut lines) = mpsc::channel(8);
        let task = tokio::spawn(pump_lines(reader, sender));

        assert_eq!(lines.recv().await, Some(LogItem::Line("hello".to_string())));
        assert_eq!(lines.recv().await, Some(LogItem::Line("world".to_string())));
        assert_eq!(lines.recv().await, None);
        task.await.expect("pump task");
    }

    /// Context used by integration tests for the kind cluster.
    const KIND_CONTEXT: &str = "kind-k8s-gpui-dev";

    fn kubeconfig_present() -> bool {
        crate::cluster::kubeconfig_present()
    }

    /// Keep only [`KIND_CONTEXT`]. Other kubeconfig contexts are not used.
    async fn registry() -> Option<crate::cluster::ClusterRegistry> {
        let mut kubeconfig = kube::config::Kubeconfig::read().ok()?;
        kubeconfig
            .contexts
            .retain(|named| named.name == KIND_CONTEXT);
        kubeconfig.current_context = Some(KIND_CONTEXT.to_string());
        Some(crate::cluster::ClusterRegistry::from_kubeconfig(kubeconfig).await)
    }

    async fn collect_lines(stream: &mut LogStream) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                LogItem::Line(line) => lines.push(line),
                LogItem::Error(reason) => panic!("log read failed: {reason}"),
            }
        }
        lines
    }

    fn kubectl(args: &[&str]) -> String {
        let output = std::process::Command::new("kubectl")
            .args(args)
            .output()
            .expect("kubectl is executable");
        assert!(
            output.status.success(),
            "kubectl {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn kubectl_tail(namespace: &str, pod: &str, tail: i64) -> Vec<String> {
        let tail_flag = format!("--tail={tail}");
        kubectl(&["logs", "-n", namespace, pod, &tail_flag])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    #[ignore = "Requires kind and kubectl: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn kind_log_stream_matches_kubectl_tail() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();

        // The perf namespace contains a Pod whose pause container has no output. Compare the empty result with kubectl.
        let perf: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "perf", &pod_resource());
        let pods = perf
            .list(&ListParams::default())
            .await
            .expect("perf is listable");
        let pod = pods.items.first().expect("perf has at least one Pod");
        let name = pod.metadata.name.clone().expect("pod name");

        let mut stream = log_stream(
            client,
            &pod_resource(),
            Some("perf"),
            &name,
            &LogOptions {
                tail_lines: Some(5),
                ..LogOptions::default()
            },
        )
        .await
        .expect("log stream");
        let lines = collect_lines(&mut stream).await;
        let expected = kubectl_tail("perf", &name, 5);
        eprintln!("[perf] {name} tail=5 ours={lines:?} kubectl={expected:?}");
        assert_eq!(lines, expected);
        // Compare non-empty output from the kind coredns Pod.
        // A tail sample can change at startup, so either kubectl sample is valid.
        let all: Api<DynamicObject> = Api::all_with(client.clone(), &pod_resource());
        let pods = all.list(&ListParams::default()).await.expect("list Pods");
        let Some(coredns) = pods.items.iter().find(|pod| {
            pod.metadata.namespace.as_deref() == Some("kube-system")
                && pod
                    .metadata
                    .name
                    .as_deref()
                    .is_some_and(|name| name.starts_with("coredns"))
        }) else {
            return;
        };
        let name = coredns.metadata.name.clone().expect("pod name");
        let before = kubectl_tail("kube-system", &name, 5);
        let mut stream = log_stream(
            client,
            &pod_resource(),
            Some("kube-system"),
            &name,
            &LogOptions {
                tail_lines: Some(5),
                ..LogOptions::default()
            },
        )
        .await
        .expect("log stream");
        let lines = collect_lines(&mut stream).await;
        let after = kubectl_tail("kube-system", &name, 5);
        eprintln!(
            "[coredns] {name} tail=5 ours={lines:?} kubectl(before)={before:?} kubectl(after)={after:?}"
        );
        assert!(!lines.is_empty(), "coredns has logs");
        assert!(
            lines == before || lines == after,
            "output differs from kubectl:\nbefore={before:?}\nafter={after:?}\nours={lines:?}"
        );
    }

    #[tokio::test]
    #[ignore = "Requires kind. Creates and deletes the two-container Pod k8s-gpui-log-probe."]
    async fn kind_log_stream_requires_container_for_multi_container_pod() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();
        let pods: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &pod_resource());
        let name = "k8s-gpui-log-probe";
        let _ = pods.delete(name, &DeleteParams::default()).await;

        let pod: DynamicObject = serde_yaml_ng::from_str(&format!(
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: {name}\n  namespace: default\nspec:\n  containers:\n    - name: alpha\n      image: registry.k8s.io/pause:3.10\n    - name: beta\n      image: registry.k8s.io/pause:3.10\n"
        ))
        .expect("probe pod yaml");
        pods.create(&kube::api::PostParams::default(), &pod)
            .await
            .expect("create probe Pod");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let current = pods.get(name).await.expect("get probe Pod");
            if current.data.pointer("/status/phase") == Some(&json!("Running")) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "probe Pod was not Running within 60 seconds"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let error = log_stream(
            client,
            &pod_resource(),
            Some("default"),
            name,
            &LogOptions::default(),
        )
        .await
        .expect_err("multi-container Pod requires container");
        match error {
            OpsError::ContainerSelectionRequired { containers, .. } => {
                eprintln!("[multi-container] containers={containers:?}");
                assert_eq!(containers, vec!["alpha", "beta"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let mut stream = log_stream(
            client,
            &pod_resource(),
            Some("default"),
            name,
            &LogOptions {
                container: Some("alpha".to_string()),
                ..LogOptions::default()
            },
        )
        .await
        .expect("log stream works with a selected container");
        assert!(
            stream.next().await.is_none(),
            "pause container has no logs. The stream ends normally"
        );

        pods.delete(name, &DeleteParams::default())
            .await
            .expect("clean up probe Pod");
    }

    #[tokio::test]
    #[ignore = "Requires kind. Creates and deletes test Events and ConfigMaps."]
    async fn kind_describe_and_apply_conflict_roundtrip() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();

        // Describe a real Pod and an Event injected by UID.
        let perf: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "perf", &pod_resource());
        let pods = perf
            .list(&ListParams::default())
            .await
            .expect("perf is listable");
        let pod = pods.items.first().expect("perf has at least one Pod");
        let name = pod.metadata.name.clone().expect("pod name");
        let uid = pod.metadata.uid.clone().expect("pod uid");

        let events: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "perf", &events_resource());
        let prefix = format!("k8s-gpui-test-{}", std::process::id());
        let newer = format!("{prefix}-newer");
        let older = format!("{prefix}-older");
        let other = format!("{prefix}-other");
        for event_name in [&newer, &older, &other] {
            let _ = events.delete(event_name, &DeleteParams::default()).await;
        }
        // Create the later event first. list_events_for sorts by lastTimestamp.
        let event = |event_name: &str, event_uid: &str, last: &str| -> DynamicObject {
            serde_yaml_ng::from_str(&format!(
                "apiVersion: v1\nkind: Event\nmetadata:\n  name: {event_name}\n  namespace: perf\ninvolvedObject:\n  kind: Pod\n  name: {name}\n  namespace: perf\n  uid: {event_uid}\nreason: Probe\nmessage: probe event\ntype: Normal\nfirstTimestamp: \"2026-09-23T08:00:00Z\"\nlastTimestamp: \"{last}\"\ncount: 1\nsource:\n  component: k8s-gpui-test\n"
            ))
            .expect("event yaml")
        };
        for probe in [
            (newer.as_str(), uid.as_str(), "2026-09-23T09:00:00Z"),
            (older.as_str(), uid.as_str(), "2026-09-23T08:30:00Z"),
            (
                other.as_str(),
                "00000000-0000-0000-0000-0000000000ff",
                "2026-09-23T07:00:00Z",
            ),
        ] {
            events
                .create(
                    &kube::api::PostParams::default(),
                    &event(probe.0, probe.1, probe.2),
                )
                .await
                .expect("create test Event");
        }

        let described = describe(client, &pod_resource(), Some("perf"), &name)
            .await
            .expect("describe");
        assert_eq!(
            described.object.metadata.name.as_deref(),
            Some(name.as_str())
        );
        assert_eq!(
            described.owners.first(),
            Some(&(
                "ReplicaSet".to_string(),
                name.rsplit_once('-').expect("rs name").0.to_string()
            ))
        );
        let matched = described
            .events
            .iter()
            .filter(|event| event.metadata.name.as_deref() == Some(newer.as_str()))
            .count();
        eprintln!(
            "[describe] {name} owners={:?} events={} matched={matched}",
            described.owners,
            described.events.len()
        );
        assert_eq!(matched, 1);

        let listed = list_events_for(client, Some("perf"), &uid)
            .await
            .expect("events");
        let names: Vec<&str> = listed
            .iter()
            .filter_map(|event| event.metadata.name.as_deref())
            .filter(|event_name| event_name.starts_with(&prefix))
            .collect();
        assert_eq!(
            names,
            vec![older.as_str(), newer.as_str()],
            "sort by lastTimestamp ascending"
        );
        let all_namespaces = list_events_for(client, None, &uid).await.expect("events");
        assert!(
            all_namespaces
                .iter()
                .any(|event| event.metadata.name.as_deref() == Some(newer.as_str()))
        );

        for event_name in [&newer, &older, &other] {
            events
                .delete(event_name, &DeleteParams::default())
                .await
                .expect("clean up test Event");
        }

        // Let alice own .data.a, then submit a different value as k8s-gpui. The apply must conflict.
        let configmaps: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &configmap_resource());
        let cm_name = "k8s-gpui-apply-probe";
        let _ = configmaps.delete(cm_name, &DeleteParams::default()).await;
        configmaps
            .patch(
                cm_name,
                &PatchParams::apply("alice"),
                &Patch::Apply(json!({
                    "apiVersion": "v1",
                    "kind": "ConfigMap",
                    "metadata": {"name": cm_name, "namespace": "default"},
                    "data": {"a": "alice"}
                })),
            )
            .await
            .expect("alice apply");
        let current = configmaps.get(cm_name).await.expect("read ConfigMap UID");
        let cm_uid = current.metadata.uid.clone().expect("ConfigMap UID");
        let yaml = format!(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {cm_name}\n  namespace: default\n  uid: {cm_uid}\ndata:\n  a: bob\n"
        );
        match apply_yaml(
            client,
            &configmap_resource(),
            Some("default"),
            &cm_uid,
            &yaml,
        )
        .await
        .expect("apply")
        {
            ApplyOutcome::Conflict { owners } => {
                assert_eq!(owners, vec!["alice".to_string()]);
            }
            ApplyOutcome::Applied(_) => panic!("expected field manager conflict"),
            ApplyOutcome::Unknown { reason } => panic!("unexpected unknown result: {reason}"),
        }

        let yaml_without_conflict = format!(
            "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {cm_name}\n  namespace: default\n  uid: {cm_uid}\ndata:\n  b: bob\n"
        );
        match apply_yaml(
            client,
            &configmap_resource(),
            Some("default"),
            &cm_uid,
            &yaml_without_conflict,
        )
        .await
        .expect("apply")
        {
            ApplyOutcome::Applied(applied) => {
                assert_eq!(applied.metadata.name.as_deref(), Some(cm_name));
            }
            ApplyOutcome::Conflict { owners } => panic!("unexpected conflict: {owners:?}"),
            ApplyOutcome::Unknown { reason } => panic!("unexpected unknown result: {reason}"),
        }
        configmaps
            .delete(cm_name, &DeleteParams::default())
            .await
            .expect("clean up ConfigMap");
    }

    /// YAML for the busybox probe Pod. The kind node caches busybox:1.36, so it does not pull the image.
    fn probe_pod(name: &str, command: &[&str]) -> DynamicObject {
        serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {
                "containers": [{
                    "name": "probe",
                    "image": "busybox:1.36",
                    "imagePullPolicy": "IfNotPresent",
                    "command": command,
                }]
            }
        }))
        .expect("probe pod json")
    }

    async fn delete_pod_and_wait(pods: &Api<DynamicObject>, name: &str) {
        let params = DeleteParams {
            grace_period_seconds: Some(0),
            ..DeleteParams::default()
        };
        let _ = pods.delete(name, &params).await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            match pods.get_opt(name).await {
                Ok(None) => return,
                Ok(Some(_)) => tokio::time::sleep(std::time::Duration::from_millis(300)).await,
                Err(_) => return,
            }
        }
        panic!("Pod {name} was not deleted within 60 seconds");
    }

    async fn ensure_running_pod(pods: &Api<DynamicObject>, pod: DynamicObject) {
        let name = pod.metadata.name.clone().expect("pod name");
        delete_pod_and_wait(pods, &name).await;
        pods.create(&kube::api::PostParams::default(), &pod)
            .await
            .expect("create probe Pod");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            let current = pods.get(&name).await.expect("get probe Pod");
            if current.data.pointer("/status/phase") == Some(&json!("Running")) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "probe Pod {name} was not Running within 120 seconds: {:?}",
                current.data.pointer("/status/phase")
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn read_stdout(reader: Option<Box<dyn AsyncRead + Send + Unpin>>) -> String {
        use tokio::io::AsyncReadExt;
        let Some(mut reader) = reader else {
            return String::new();
        };
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.expect("read stdout");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn curl_body(port: u16) -> String {
        let output = std::process::Command::new("curl")
            .args([
                "-sS",
                "--max-time",
                "10",
                &format!("http://127.0.0.1:{port}/"),
            ])
            .output()
            .expect("curl is executable");
        assert!(
            output.status.success(),
            "curl 127.0.0.1:{port} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[tokio::test]
    #[ignore = "Requires kind and kubectl. Creates and deletes the busybox probe Pod k8s-gpui-exec-probe."]
    async fn kind_exec_matches_kubectl_and_reports_exit_code() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();
        let pods: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &pod_resource());
        let name = "k8s-gpui-exec-probe";
        ensure_running_pod(&pods, probe_pod(name, &["sh", "-c", "sleep 3600"])).await;

        // Non-TTY mode keeps stdout and stderr separate for comparison with kubectl exec.
        let options = ExecOptions {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo hello; echo err >&2".to_string(),
            ],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        assert!(session.stdin().is_none(), "non-TTY execution is read-only");
        let stdout = read_stdout(session.take_stdout()).await;
        let stderr = read_stdout(session.take_stderr()).await;
        let status = session.status().await.expect("status");
        let expected = kubectl(&[
            "exec",
            "-n",
            "default",
            name,
            "--",
            "sh",
            "-c",
            "echo hello; echo err >&2",
        ]);
        eprintln!("[exec] stdout={stdout:?} stderr={stderr:?} status={status:?}");
        assert_eq!(stdout, "hello\n");
        assert_eq!(stdout, expected, "stdout matches kubectl exec");
        assert_eq!(stderr, "err\n");
        assert_eq!(status.code, 0);
        assert_eq!(session.status().await.expect("cached status").code, 0);

        // A nonzero exit code is returned through status, not as an error.
        let options = ExecOptions {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo boom >&2; exit 7".to_string(),
            ],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        let stderr = read_stdout(session.take_stderr()).await;
        let status = session.status().await.expect("status");
        eprintln!("[exec] exit7 status={status:?}");
        assert_eq!(stderr, "boom\n");
        assert_eq!(status.code, 7);

        // kube 4.2 has no env flag, so the command uses an env prefix.
        let options = ExecOptions {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo $K8S_GPUI_PROBE".to_string(),
            ],
            env: vec![("K8S_GPUI_PROBE".to_string(), "env-ok".to_string())],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        let stdout = read_stdout(session.take_stdout()).await;
        assert_eq!(stdout, "env-ok\n");

        // A missing container must produce a readable error.
        let options = ExecOptions {
            container: Some("nope".to_string()),
            command: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
            ..ExecOptions::default()
        };
        let error = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect_err("missing container must fail");
        eprintln!("[exec] missing container error={error}");
        match error {
            OpsError::ContainerNotFound {
                container,
                containers,
                ..
            } => {
                assert_eq!(container, "nope");
                assert_eq!(containers, vec!["probe".to_string()]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        // Cancellation aborts immediately. Later sessions remain usable.
        let options = ExecOptions {
            command: vec!["sh".to_string(), "-c".to_string(), "sleep 600".to_string()],
            ..ExecOptions::default()
        };
        let session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        session.cancel();
        let options = ExecOptions {
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo cancel-ok".to_string(),
            ],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        assert_eq!(read_stdout(session.take_stdout()).await, "cancel-ok\n");

        // After the Pod is deleted, status() ends with an exit code or ExecDisconnected.
        let options = ExecOptions {
            command: vec!["sh".to_string(), "-c".to_string(), "sleep 600".to_string()],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec");
        delete_pod_and_wait(&pods, name).await;
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(60), session.status())
            .await
            .expect("status ends after the Pod is deleted");
        match outcome {
            Ok(status) => eprintln!("[exec] pod deleted, status={status:?}"),
            Err(OpsError::ExecDisconnected) => {}
            Err(other) => panic!("unexpected error: {other:?}"),
        }

        delete_pod_and_wait(&pods, name).await;
    }

    #[tokio::test]
    #[ignore = "Requires kind. Creates and deletes the busybox probe Pod k8s-gpui-exec-tty-probe."]
    async fn kind_exec_tty_merges_stderr_and_resizes() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();
        let pods: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &pod_resource());
        let name = "k8s-gpui-exec-tty-probe";
        ensure_running_pod(&pods, probe_pod(name, &["sh", "-c", "sleep 3600"])).await;

        let options = ExecOptions {
            tty: true,
            command: vec!["sh".to_string()],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("tty exec");
        assert!(
            session.stderr().is_none(),
            "TTY mode merges stderr into stdout. It cannot open stderr separately"
        );
        session
            .resize(TerminalSize {
                width: 120,
                height: 40,
            })
            .expect("resize");
        {
            use tokio::io::AsyncWriteExt;
            let stdin = session.stdin().expect("TTY has stdin");
            stdin
                .write_all(b"echo tty-ok; echo err-merged >&2; exit\n")
                .await
                .expect("write stdin");
        }
        let stdout = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            read_stdout(session.take_stdout()),
        )
        .await
        .expect("TTY output reaches EOF within 30 seconds");
        eprintln!("[exec-tty] output={stdout:?}");
        assert!(stdout.contains("tty-ok"), "TTY output: {stdout:?}");
        assert!(
            stdout.contains("err-merged"),
            "stderr is merged into stdout: {stdout:?}"
        );
        assert_eq!(session.status().await.expect("status").code, 0);

        delete_pod_and_wait(&pods, name).await;
    }

    #[tokio::test]
    #[ignore = "Requires kind. Creates and deletes the two-container Pod k8s-gpui-exec-multi-probe."]
    async fn kind_exec_requires_container_for_multi_container_pod() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();
        let pods: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &pod_resource());
        let name = "k8s-gpui-exec-multi-probe";
        let pod: DynamicObject = serde_json::from_value(json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": name, "namespace": "default"},
            "spec": {
                "containers": [
                    {
                        "name": "alpha",
                        "image": "busybox:1.36",
                        "imagePullPolicy": "IfNotPresent",
                        "command": ["sh", "-c", "sleep 3600"],
                    },
                    {
                        "name": "beta",
                        "image": "registry.k8s.io/pause:3.10",
                        "imagePullPolicy": "IfNotPresent",
                    }
                ]
            }
        }))
        .expect("multi pod json");
        ensure_running_pod(&pods, pod).await;

        let options = ExecOptions {
            command: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
            ..ExecOptions::default()
        };
        let error = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect_err("multi-container Pod requires container");
        match error {
            OpsError::ContainerSelectionRequired { containers, .. } => {
                assert_eq!(containers, vec!["alpha", "beta"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let options = ExecOptions {
            container: Some("alpha".to_string()),
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo alpha-ok".to_string(),
            ],
            ..ExecOptions::default()
        };
        let mut session = exec(client, &pod_resource(), Some("default"), name, &options)
            .await
            .expect("exec works with a selected container");
        assert_eq!(read_stdout(session.take_stdout()).await, "alpha-ok\n");

        delete_pod_and_wait(&pods, name).await;
    }

    // curl blocks. A current-thread runtime can starve the forwarding task, so use multiple threads.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Requires kind and curl. Creates and deletes the busybox probe Pod k8s-gpui-fwd-probe."]
    async fn kind_port_forward_roundtrip_multi_port_and_cleanup() {
        if !kubeconfig_present() {
            return;
        }
        let Some(registry) = registry().await else {
            return;
        };
        let Some(cluster) = registry.clusters().first() else {
            return;
        };
        let client = cluster.client();
        let pods: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), "default", &pod_resource());
        let name = "k8s-gpui-fwd-probe";
        ensure_running_pod(
            &pods,
            probe_pod(
                name,
                &[
                    "sh",
                    "-c",
                    "mkdir -p /www; echo fwd-ok > /www/index.html; \
                     httpd -f -p 8080 -h /www & httpd -f -p 8081 -h /www & wait",
                ],
            ),
        )
        .await;

        let session = PortForwardSession::start(
            client,
            &pod_resource(),
            Some("default"),
            name,
            vec![8080, 8081],
        )
        .await
        .expect("port-forward");
        let locals = session.local_ports();
        assert_eq!(locals.len(), 2);

        // Multiple ports run concurrently. Compare each local port with curl.
        assert_eq!(curl_body(locals[0]), "fwd-ok\n");
        assert_eq!(curl_body(locals[1]), "fwd-ok\n");

        // The local port is released after stop. It can bind again.
        let local_8080 = locals[0];
        session.stop();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        std::net::TcpListener::bind(("127.0.0.1", local_8080))
            .expect("the local port must be released after stop");

        // An unlisted port is reported by kubelet through the error channel.
        let mut bad =
            PortForwardSession::start(client, &pod_resource(), Some("default"), name, vec![9999])
                .await
                .expect("start bad port");
        let bad_local = bad.local_ports()[0];
        let _ = tokio::net::TcpStream::connect(("127.0.0.1", bad_local)).await;
        let error = tokio::time::timeout(std::time::Duration::from_secs(15), bad.next_error())
            .await
            .expect("receive the port error within 15 seconds");
        let error = error.expect("session ended early");
        eprintln!("[port-forward] unavailable port error={error:?}");
        assert_eq!(error.port, 9999);
        assert!(!error.reason.is_empty());

        delete_pod_and_wait(&pods, name).await;
    }
}
