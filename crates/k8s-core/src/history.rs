//! Persists the last spec of every object this app applied, in `history.json`.
//!
//! only objects the user actually edited are recorded, only the most
//! recent spec per object is kept, and an entry is written only after the API server accepted
//! the apply — what is stored is the spec that worked before this one, so a later apply can be
//! reverted to it. This is deliberately not a watch-event log (D22): nothing is recorded until
//! an apply succeeds, and nothing about an untouched object is ever read.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::atomic_file::{read_json_if_present, write_json_atomic};
use crate::paths;

const FORMAT_VERSION: u32 = 1;

/// Stands in for the namespace of a cluster-scoped object. A namespace name is a DNS-1123 label
/// and can never be `-`, so the segment cannot collide with a real one.
const CLUSTER_SCOPED: &str = "-";

/// Serializes the read-modify-write of the file, so two applies finishing at the same time
/// cannot both start from the same contents and drop one entry.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error(
        "Apply history read or write failed: {0}. Check the configuration directory and try again."
    )]
    Io(#[from] std::io::Error),

    #[error("Apply history JSON processing failed: {0}. Check the file and try again.")]
    Json(#[from] serde_json::Error),

    #[error("Unsupported apply history file version {found}. Use a supported version.")]
    UnsupportedVersion { found: u32 },

    #[error(
        "The configuration directory is unavailable. Set HOME or XDG_CONFIG_HOME and try again."
    )]
    NoConfigDir,
}

/// One object the apply history tracks. The file key is
/// `<cluster>/<namespace>/<kind>/<name>`, with `-` for a cluster-scoped object.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HistoryTarget {
    cluster: String,
    namespace: Option<String>,
    kind: String,
    name: String,
}

impl HistoryTarget {
    pub fn new(cluster: &str, namespace: Option<&str>, kind: &str, name: &str) -> Self {
        Self {
            cluster: cluster.to_owned(),
            namespace: namespace.map(str::to_owned),
            kind: kind.to_owned(),
            name: name.to_owned(),
        }
    }

    /// The `entries` key this target is stored under.
    pub fn key(&self) -> String {
        format!(
            "{}/{}/{}/{}",
            self.cluster,
            self.namespace.as_deref().unwrap_or(CLUSTER_SCOPED),
            self.kind,
            self.name
        )
    }
}

/// One stored spec.
///
/// `saved_at` is RFC 3339 in UTC to whole seconds (`2026-09-28T12:04:11Z`); parse it with
/// `chrono::DateTime::parse_from_rfc3339` rather than slicing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub saved_at: String,
    /// The complete object as the server held it before the apply, so a revert can be built
    /// from it without another request.
    pub spec: Value,
    /// The label of the action that started the apply, e.g. `⌘K → Edit YAML`.
    pub applied_by: String,
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    entries: BTreeMap<String, HistoryEntry>,
}

impl HistoryFile {
    fn resolve(self) -> Result<HistoryStore, HistoryError> {
        if self.version != FORMAT_VERSION {
            return Err(HistoryError::UnsupportedVersion {
                found: self.version,
            });
        }
        Ok(HistoryStore {
            entries: self.entries,
        })
    }
}

/// The stored specs, in memory. Loading once makes "what was applied here before?" a map
/// lookup, which the inspector asks on every selection.
#[derive(Clone, Debug, Default)]
pub struct HistoryStore {
    entries: BTreeMap<String, HistoryEntry>,
}

impl HistoryStore {
    /// Load from disk. A missing file returns an empty store. Invalid data returns an error.
    pub fn load(path: impl AsRef<Path>) -> (Self, Option<HistoryError>) {
        match read_json_if_present::<HistoryFile, HistoryError>(path.as_ref()) {
            Ok(None) => (Self::default(), None),
            Ok(Some(file)) => match file.resolve() {
                Ok(store) => (store, None),
                Err(error) => (Self::default(), Some(error)),
            },
            Err(error) => (Self::default(), Some(error)),
        }
    }

    pub fn load_default() -> (Self, Option<HistoryError>) {
        match paths::history_file() {
            Some(path) => Self::load(path),
            None => (Self::default(), Some(HistoryError::NoConfigDir)),
        }
    }

    /// The spec that was live before the most recent successful apply, if there was one.
    pub fn previous(&self, target: &HistoryTarget) -> Option<&HistoryEntry> {
        self.entries.get(&target.key())
    }

    /// Write the store atomically. [`record_previous_spec`] is the usual way in.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), HistoryError> {
        let path = path.as_ref();
        write_json_atomic(
            path,
            &HistoryFile {
                version: FORMAT_VERSION,
                entries: self.entries.clone(),
            },
        )
    }
}

/// Store the spec an apply overwrote, replacing any earlier entry for the same object, and write
/// the file atomically.
///
/// Called only after the API server accepted the apply: a failed apply changed nothing, so the
/// spec it would have replaced is still the live one. An unreadable file is reported, not
/// overwritten — it holds other objects' history and refusing to lose it is worth more than one
/// snapshot.
pub fn record_previous_spec(
    path: impl AsRef<Path>,
    target: &HistoryTarget,
    spec: Value,
    applied_by: &str,
) -> Result<(), HistoryError> {
    let path = path.as_ref();
    let entry = HistoryEntry {
        saved_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        spec,
        applied_by: applied_by.to_owned(),
    };
    let _guard = WRITE_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let (mut store, error) = HistoryStore::load(path);
    if let Some(error) = error {
        return Err(error);
    }
    store.entries.insert(target.key(), entry);
    store.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("k8s-gpui-history-{}-{name}", std::process::id()));
        fs::create_dir_all(&directory).expect("create temporary directory");
        directory.join("history.json")
    }

    fn spec(replicas: i64) -> Value {
        serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "Deployment",
            "metadata": { "name": "api", "namespace": "api", "uid": "uid-api" },
            "spec": { "replicas": replicas }
        })
    }

    fn target(name: &str) -> HistoryTarget {
        HistoryTarget::new("000000000000000a", Some("api"), "Deployment", name)
    }

    /// The on-disk shape is a contract with C8's revert flow and with anyone reading the file
    /// by hand, so it is asserted as written rather than through the store's own accessors.
    #[test]
    fn round_trips_the_documented_file_shape() {
        let path = temp_file("round-trip");
        let _ = fs::remove_file(&path);

        record_previous_spec(&path, &target("api"), spec(1), "⌘K → Edit YAML").expect("record");

        let on_disk: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("json");
        assert_eq!(on_disk["version"], json!(1));
        let entries = on_disk["entries"].as_object().expect("entries object");
        assert_eq!(entries.len(), 1, "one entry per edited object");
        let entry = &entries["000000000000000a/api/Deployment/api"];
        assert_eq!(entry["spec"], spec(1));
        assert_eq!(entry["appliedBy"], json!("⌘K → Edit YAML"));
        let saved_at = entry["savedAt"].as_str().expect("savedAt");
        assert!(saved_at.ends_with('Z'), "savedAt is UTC: {saved_at}");
        assert_eq!(
            saved_at.len(),
            20,
            "savedAt is RFC 3339 to whole seconds: {saved_at}"
        );
        chrono::DateTime::parse_from_rfc3339(saved_at).expect("savedAt is RFC 3339");

        let (store, error) = HistoryStore::load(&path);
        assert!(error.is_none(), "{error:?}");
        let stored = store.previous(&target("api")).expect("entry round trips");
        assert_eq!(stored.spec, spec(1));
        assert_eq!(stored.applied_by, "⌘K → Edit YAML");

        assert_eq!(
            HistoryTarget::new("000000000000000a", None, "Node", "node-1").key(),
            "000000000000000a/-/Node/node-1",
            "a cluster-scoped object has no namespace segment to collide with"
        );
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }

    /// Only the most recent spec per object is kept: an older one left behind would offer C8 a
    /// revert target the user never saw.
    #[test]
    fn keeps_only_the_most_recent_spec_per_object() {
        let path = temp_file("most-recent");
        let _ = fs::remove_file(&path);

        record_previous_spec(&path, &target("api"), spec(1), "⌘K → Edit YAML").expect("record");
        record_previous_spec(&path, &target("web"), spec(9), "Restart").expect("record");
        record_previous_spec(&path, &target("api"), spec(3), "⌘⇧↵ Apply").expect("record");

        let (store, error) = HistoryStore::load(&path);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(store.entries.len(), 2, "the replaced entry is gone");
        let api = store.previous(&target("api")).expect("api entry");
        assert_eq!(api.spec, spec(3), "the newest spec replaces the older one");
        assert_eq!(api.applied_by, "⌘⇧↵ Apply");
        assert!(
            store.previous(&target("worker")).is_none(),
            "an object that was never applied has no history"
        );
        assert_eq!(
            store.previous(&target("web")).expect("web entry").spec,
            spec(9),
            "a second object keeps its own entry"
        );
        let _ = fs::remove_dir_all(path.parent().expect("temp dir"));
    }
}
