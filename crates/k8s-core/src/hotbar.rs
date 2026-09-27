//! Persists hotbar banks and cluster slots in hotbar.json.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_file::write_json_atomic;
use crate::cluster::{ClusterId, ClusterRegistry};
use crate::paths;

/// Maximum number of slots in one bank.
pub const MAX_SLOTS_PER_BANK: usize = 12;

const FORMAT_VERSION: u32 = 1;
const FILE_NAME: &str = "hotbar.json";

#[derive(Debug, thiserror::Error)]
pub enum HotbarError {
    #[error("Hotbar read or write failed: {0}. Check the configuration directory and try again.")]
    Io(#[from] std::io::Error),

    #[error("Hotbar JSON processing failed: {0}. Check the file and try again.")]
    Json(#[from] serde_json::Error),

    #[error("Unsupported hotbar file version {found}. Use a supported version.")]
    UnsupportedVersion { found: u32 },

    #[error("Invalid hotbar file content: {reason}. Correct the file and try again.")]
    Invalid { reason: String },

    #[error(
        "The configuration directory is unavailable. Set HOME or XDG_CONFIG_HOME and try again."
    )]
    NoConfigDir,

    #[error("Bank name must not be empty. Enter a name.")]
    EmptyBankName,

    #[error("Bank name already exists: {name}. Choose another name.")]
    DuplicateBank { name: String },

    #[error("Bank index {index} is out of range. Choose an existing bank.")]
    NoSuchBank { index: usize },

    #[error("Slot index {slot} is out of range for bank {bank}. Choose an existing slot.")]
    NoSuchSlot { bank: usize, slot: usize },

    #[error("Bank {bank} is full. It has {max} slots. Remove a slot and try again.")]
    BankFull { bank: String, max: usize },

    #[error(
        "Hotbar slot cannot be resolved: cluster_id={cluster_id}, label={label}. Refresh the cluster list and try again."
    )]
    UnresolvedSlot { cluster_id: String, label: String },

    #[error("The cluster is already in bank {bank}. Choose another bank.")]
    DuplicateCluster { bank: String },
}

/// The complete hotbar with its banks and active index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hotbar {
    pub banks: Vec<Bank>,
    /// Index into `banks`. It is zero when the list is empty.
    pub active: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bank {
    pub name: String,
    pub slots: Vec<Slot>,
}

/// A cluster slot with a stable ID and display label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub cluster_id: ClusterId,
    pub label: String,
}

impl Slot {
    pub fn new(cluster_id: ClusterId, label: impl Into<String>) -> Self {
        Self {
            cluster_id,
            label: label.into(),
        }
    }

    /// Return true when the slot points to this cluster.
    pub fn matches(&self, cluster_id: ClusterId) -> bool {
        self.cluster_id == cluster_id
    }
}

impl Bank {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            slots: Vec::new(),
        }
    }
}

/// Default hotbar path under the configuration directory.
pub fn default_path() -> Option<PathBuf> {
    paths::config_file(FILE_NAME)
}

#[derive(Clone, Debug, Default)]
pub struct ClusterIndex {
    ids: HashMap<String, ClusterId>,
}

impl ClusterIndex {
    pub fn get(&self, key: &str) -> Option<&ClusterId> {
        self.ids.get(key)
    }
}

/// Map stable cluster IDs from the current registry.
pub fn cluster_index(registry: &ClusterRegistry) -> ClusterIndex {
    let mut index = ClusterIndex::default();
    for cluster in registry.clusters() {
        index.ids.insert(cluster.id().to_string(), cluster.id());
    }
    index
}

impl Hotbar {
    /// Load from disk. A missing file returns an empty hotbar. Invalid data returns an error.
    pub fn load(
        path: impl AsRef<Path>,
        resolve: impl Fn(&str) -> Option<ClusterId>,
    ) -> (Self, Option<HotbarError>) {
        match read_file(path.as_ref()) {
            Ok(None) => (Self::default(), None),
            Ok(Some(file)) => match file.resolve(&resolve) {
                Ok(hotbar) => (hotbar, None),
                Err(error) => (Self::default(), Some(error)),
            },
            Err(error) => (Self::default(), Some(error)),
        }
    }

    pub fn load_default(
        resolve: impl Fn(&str) -> Option<ClusterId>,
    ) -> (Self, Option<HotbarError>) {
        match default_path() {
            Some(path) => Self::load(path, resolve),
            None => (Self::default(), Some(HotbarError::NoConfigDir)),
        }
    }

    /// Save atomically with a private temporary file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), HotbarError> {
        let path = path.as_ref();
        write_json_atomic(path, &HotbarFile::from_hotbar(self))
    }

    pub fn save_default(&self) -> Result<(), HotbarError> {
        let path = default_path().ok_or(HotbarError::NoConfigDir)?;
        self.save(path)
    }

    pub fn bank(&self, index: usize) -> Option<&Bank> {
        self.banks.get(index)
    }

    pub fn active_bank(&self) -> Option<&Bank> {
        self.banks.get(self.active)
    }

    /// Add a bank and return its index without changing `active`.
    pub fn create_bank(&mut self, name: impl Into<String>) -> Result<usize, HotbarError> {
        let name = trimmed(name.into());
        if name.is_empty() {
            return Err(HotbarError::EmptyBankName);
        }
        if self.banks.iter().any(|bank| bank.name == name) {
            return Err(HotbarError::DuplicateBank { name });
        }
        self.banks.push(Bank::new(name));
        Ok(self.banks.len() - 1)
    }

    /// Rename a bank. Names must be unique after trimming.
    pub fn rename_bank(
        &mut self,
        index: usize,
        name: impl Into<String>,
    ) -> Result<(), HotbarError> {
        if index >= self.banks.len() {
            return Err(HotbarError::NoSuchBank { index });
        }
        let name = trimmed(name.into());
        if name.is_empty() {
            return Err(HotbarError::EmptyBankName);
        }
        if self
            .banks
            .iter()
            .enumerate()
            .any(|(position, bank)| position != index && bank.name == name)
        {
            return Err(HotbarError::DuplicateBank { name });
        }
        self.banks[index].name = name;
        Ok(())
    }

    /// Remove a bank and adjust `active`.
    pub fn remove_bank(&mut self, index: usize) -> Result<Bank, HotbarError> {
        if index >= self.banks.len() {
            return Err(HotbarError::NoSuchBank { index });
        }
        let bank = self.banks.remove(index);
        if index < self.active {
            self.active -= 1;
        } else if self.active >= self.banks.len() {
            self.active = self.banks.len().saturating_sub(1);
        }
        Ok(bank)
    }

    /// Add a cluster slot. A cluster can appear once per bank.
    pub fn add_slot(
        &mut self,
        bank: usize,
        cluster_id: ClusterId,
        label: impl Into<String>,
    ) -> Result<usize, HotbarError> {
        let bank_ref = self
            .banks
            .get_mut(bank)
            .ok_or(HotbarError::NoSuchBank { index: bank })?;
        if bank_ref.slots.len() >= MAX_SLOTS_PER_BANK {
            return Err(HotbarError::BankFull {
                bank: bank_ref.name.clone(),
                max: MAX_SLOTS_PER_BANK,
            });
        }
        if bank_ref.slots.iter().any(|slot| slot.matches(cluster_id)) {
            return Err(HotbarError::DuplicateCluster {
                bank: bank_ref.name.clone(),
            });
        }
        bank_ref.slots.push(Slot::new(cluster_id, label));
        Ok(bank_ref.slots.len() - 1)
    }

    /// Set the active bank. The index must exist.
    pub fn set_active(&mut self, index: usize) -> Result<(), HotbarError> {
        if index >= self.banks.len() {
            return Err(HotbarError::NoSuchBank { index });
        }
        self.active = index;
        Ok(())
    }
}

fn trimmed(name: String) -> String {
    name.trim().to_string()
}

fn is_stable_cluster_id(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Serialize, Deserialize)]
struct HotbarFile {
    version: u32,
    active: usize,
    banks: Vec<BankFile>,
}

#[derive(Serialize, Deserialize)]
struct BankFile {
    name: String,
    slots: Vec<SlotFile>,
}

#[derive(Serialize, Deserialize)]
struct SlotFile {
    cluster_id: String,
    label: String,
}

impl HotbarFile {
    fn from_hotbar(hotbar: &Hotbar) -> Self {
        Self {
            version: FORMAT_VERSION,
            active: hotbar.active,
            banks: hotbar
                .banks
                .iter()
                .map(|bank| BankFile {
                    name: bank.name.clone(),
                    slots: bank
                        .slots
                        .iter()
                        .map(|slot| SlotFile {
                            cluster_id: slot.cluster_id.to_string(),
                            label: slot.label.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    fn resolve(self, resolve: &impl Fn(&str) -> Option<ClusterId>) -> Result<Hotbar, HotbarError> {
        if self.version != FORMAT_VERSION {
            return Err(HotbarError::UnsupportedVersion {
                found: self.version,
            });
        }
        let mut banks = Vec::with_capacity(self.banks.len());
        for bank in self.banks {
            let name = trimmed(bank.name);
            if name.is_empty() {
                return Err(HotbarError::Invalid {
                    reason: "bank name is empty".to_string(),
                });
            }
            if banks.iter().any(|existing: &Bank| existing.name == name) {
                return Err(HotbarError::DuplicateBank { name });
            }
            if bank.slots.len() > MAX_SLOTS_PER_BANK {
                return Err(HotbarError::Invalid {
                    reason: format!(
                        "bank {} has {} slots. The limit is {MAX_SLOTS_PER_BANK}.",
                        name,
                        bank.slots.len()
                    ),
                });
            }
            let mut slots = Vec::with_capacity(bank.slots.len());
            for slot in bank.slots {
                let resolved = is_stable_cluster_id(&slot.cluster_id)
                    .then(|| resolve(&slot.cluster_id))
                    .flatten();
                let Some(cluster_id) = resolved else {
                    // A slot is a convenience, not a claim about the app: one
                    // entry naming a cluster that has since left the registry
                    // must not cost the user every other slot in the bank, and
                    // must never block loading kubeconfig. Drop the slot and
                    // keep going; the cluster id stays in the log so the user
                    // can see which entry went stale.
                    tracing::warn!(
                        cluster_id = %slot.cluster_id,
                        label = %slot.label,
                        "The hotbar slot ID is not in the current cluster registry. Dropping the \
                         slot."
                    );
                    continue;
                };
                if slots.iter().any(|slot: &Slot| slot.matches(cluster_id)) {
                    return Err(HotbarError::DuplicateCluster { bank: name.clone() });
                }
                slots.push(Slot {
                    cluster_id,
                    label: slot.label,
                });
            }
            banks.push(Bank { name, slots });
        }
        let active = if banks.is_empty() {
            0
        } else {
            self.active.min(banks.len() - 1)
        };
        Ok(Hotbar { banks, active })
    }
}

fn read_file(path: &Path) -> Result<Option<HotbarFile>, HotbarError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const KUBECONFIG: &str = r#"
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
  context:
    cluster: alpha
    user: alpha-user
- name: beta-ctx
  context:
    cluster: beta
    user: beta-user
users:
- name: alpha-user
  user: {}
- name: beta-user
  user: {}
current-context: alpha-ctx
"#;

    fn id(context: &str) -> ClusterId {
        ClusterId::derive(context, "https://example.com:6443")
    }

    fn id_resolver(ids: &[ClusterId]) -> impl Fn(&str) -> Option<ClusterId> + '_ {
        move |stored_id| ids.iter().copied().find(|id| id.to_string() == stored_id)
    }

    async fn test_registry() -> ClusterRegistry {
        let kubeconfig = kube::config::Kubeconfig::from_yaml(KUBECONFIG).expect("parse kubeconfig");
        ClusterRegistry::from_kubeconfig(kubeconfig).await
    }

    async fn registry_with_beta_server(server: &str) -> ClusterRegistry {
        let yaml = KUBECONFIG.replace("http://127.0.0.1:6444", server);
        let kubeconfig = kube::config::Kubeconfig::from_yaml(&yaml).expect("parse kubeconfig");
        ClusterRegistry::from_kubeconfig(kubeconfig).await
    }

    fn registry_cluster_id(registry: &ClusterRegistry, name: &str) -> ClusterId {
        registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == name)
            .expect("context exists")
            .id()
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("k8s-gpui-hotbar-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temporary directory");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join(FILE_NAME)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn hotbar_with(ids: &[ClusterId]) -> Hotbar {
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        for (position, cluster) in ids.iter().enumerate() {
            hotbar
                .add_slot(bank, *cluster, format!("cluster-{position}"))
                .expect("add slot");
        }
        hotbar
    }

    #[test]
    fn round_trips_banks_and_slots() {
        let dir = TempDir::new("round-trip");
        let path = dir.file();
        let alpha = id("alpha");
        let beta = id("beta");
        let mut hotbar = hotbar_with(&[alpha, beta]);
        let other = hotbar.create_bank("ops").expect("create bank");
        hotbar
            .add_slot(other, alpha, "alpha")
            .expect("the same cluster can appear in different banks");
        hotbar.set_active(1).expect("switch bank");
        hotbar.save(&path).expect("save to disk");

        let raw = fs::read_to_string(&path).expect("file is readable");
        assert!(
            raw.contains(&alpha.to_string()),
            "the saved value is the hexadecimal stable ID"
        );

        let (loaded, error) = Hotbar::load(&path, id_resolver(&[alpha, beta]));
        assert!(error.is_none(), "{error:?}");
        assert_eq!(loaded, hotbar);
    }

    #[test]
    fn missing_file_is_empty_without_error() {
        let dir = TempDir::new("missing");
        let (loaded, error) = Hotbar::load(dir.file(), id_resolver(&[]));
        assert!(error.is_none());
        assert_eq!(loaded, Hotbar::default());
    }

    #[test]
    fn corrupt_or_unsupported_file_recovers_to_default() {
        let dir = TempDir::new("corrupt");
        let path = dir.file();

        fs::write(&path, b"{not json").expect("write invalid file");
        let (loaded, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(matches!(error, Some(HotbarError::Json(_))));
        assert_eq!(loaded, Hotbar::default());

        fs::write(&path, r#"{ "version": 99, "active": 0, "banks": [] }"#).expect("write file");
        let (loaded, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(matches!(
            error,
            Some(HotbarError::UnsupportedVersion { found: 99 })
        ));
        assert_eq!(loaded, Hotbar::default());
    }

    #[test]
    fn struct_validation_rejects_invalid_banks() {
        let dir = TempDir::new("invalid");
        let path = dir.file();

        let nameless =
            r#"{ "version": 1, "active": 0, "banks": [ { "name": "  ", "slots": [] } ] }"#;
        fs::write(&path, nameless).expect("write file");
        let (loaded, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(matches!(error, Some(HotbarError::Invalid { .. })));
        assert_eq!(loaded, Hotbar::default());

        let duplicate_banks = r#"{ "version": 1, "active": 0, "banks": [
            { "name": "same", "slots": [] },
            { "name": " same ", "slots": [] }
        ] }"#;
        fs::write(&path, duplicate_banks).expect("write file");
        let (loaded, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(matches!(error, Some(HotbarError::DuplicateBank { .. })));
        assert_eq!(loaded, Hotbar::default());

        let slots: Vec<String> = (0..MAX_SLOTS_PER_BANK + 1)
            .map(|position| {
                format!(r#"{{ "cluster_id": "missing-{position}", "label": "c{position}" }}"#)
            })
            .collect();
        let oversized = format!(
            r#"{{ "version": 1, "active": 0, "banks": [ {{ "name": "b", "slots": [{}] }} ] }}"#,
            slots.join(", ")
        );
        fs::write(&path, oversized).expect("write file");
        let (_, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(
            matches!(error, Some(HotbarError::Invalid { .. })),
            "reject files with more than {MAX_SLOTS_PER_BANK} slots"
        );
    }

    #[tokio::test]
    async fn load_prefers_exact_cluster_id_over_label() {
        let dir = TempDir::new("resolve-exact");
        let path = dir.file();
        let registry = test_registry().await;
        let beta = registry_cluster_id(&registry, "beta-ctx");
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        hotbar.add_slot(bank, beta, "alpha-ctx").expect("add slot");
        hotbar.save(&path).expect("save to disk");

        let index = cluster_index(&registry);
        let resolutions = Cell::new(0);
        let (loaded, error) = Hotbar::load(&path, |key| {
            resolutions.set(resolutions.get() + 1);
            index.get(key).copied()
        });
        assert!(error.is_none(), "{error:?}");
        assert_eq!(resolutions.get(), 1, "each slot queries the stable ID once");
        assert_eq!(loaded.banks[0].slots, [Slot::new(beta, "alpha-ctx")]);
    }

    #[tokio::test]
    async fn rebuilt_cluster_with_same_context_is_not_rebound_by_label() {
        let dir = TempDir::new("resolve-rebuilt");
        let path = dir.file();
        let old_registry = registry_with_beta_server("http://127.0.0.1:1").await;
        let old_id = registry_cluster_id(&old_registry, "beta-ctx");
        let registry = test_registry().await;
        let current_id = registry_cluster_id(&registry, "beta-ctx");
        assert_ne!(old_id, current_id);

        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        hotbar.add_slot(bank, old_id, "beta-ctx").expect("add slot");
        hotbar.save(&path).expect("save to disk");

        let index = cluster_index(&registry);
        let label_lookups = Cell::new(0);
        let (loaded, error) = Hotbar::load(&path, |key| {
            if key == "beta-ctx" {
                label_lookups.set(label_lookups.get() + 1);
                Some(current_id)
            } else {
                index.get(key).copied()
            }
        });
        assert_eq!(
            label_lookups.get(),
            0,
            "the context label does not select a fallback"
        );
        assert!(error.is_none(), "{error:?}");
        assert!(
            loaded.banks[0].slots.is_empty(),
            "the slot is dropped, not rebound: {loaded:?}"
        );
        assert_eq!(loaded.banks[0].name, "default");
    }

    #[tokio::test]
    async fn an_unresolved_slot_is_dropped_without_selecting_another_cluster() {
        let dir = TempDir::new("resolve-missing");
        let path = dir.file();
        let registry = test_registry().await;
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        hotbar
            .add_slot(bank, id("missing"), "missing-ctx")
            .expect("add slot");
        hotbar.save(&path).expect("save to disk");

        let index = cluster_index(&registry);
        let (loaded, error) = Hotbar::load(&path, |key| index.get(key).copied());
        assert!(error.is_none(), "{error:?}");
        assert!(loaded.banks[0].slots.is_empty(), "{loaded:?}");

        let malformed = r#"{ "version": 1, "active": 0, "banks": [ {
            "name": "default",
            "slots": [ { "cluster_id": "missing", "label": "missing-ctx" } ]
        } ] }"#;
        fs::write(&path, malformed).expect("write file");
        let (loaded, error) = Hotbar::load(&path, |key| (key == "missing").then(|| id("missing")));
        assert!(error.is_none(), "{error:?}");
        assert!(loaded.banks[0].slots.is_empty(), "{loaded:?}");
    }

    #[tokio::test]
    async fn one_stale_slot_does_not_cost_the_bank_its_live_slots() {
        let dir = TempDir::new("resolve-partial");
        let path = dir.file();
        let registry = test_registry().await;
        let live = registry_cluster_id(&registry, "alpha-ctx");
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        hotbar
            .add_slot(bank, live, "alpha-ctx")
            .expect("add live slot");
        hotbar
            .add_slot(bank, id("missing"), "missing-ctx")
            .expect("add stale slot");
        hotbar.save(&path).expect("save to disk");

        let index = cluster_index(&registry);
        let (loaded, error) = Hotbar::load(&path, |key| index.get(key).copied());
        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            loaded.banks[0].slots,
            [Slot::new(live, "alpha-ctx")],
            "the live slot survives its stale neighbour"
        );
    }

    #[tokio::test]
    async fn repeated_label_across_banks_never_auto_resolves() {
        let dir = TempDir::new("resolve-repeated-label");
        let path = dir.file();
        let registry = test_registry().await;
        let first_stale = ClusterId::derive("beta-ctx", "http://127.0.0.1:1");
        let second_stale = ClusterId::derive("beta-ctx", "http://127.0.0.1:2");
        let mut hotbar = Hotbar::default();
        let first = hotbar.create_bank("first").expect("create bank");
        let second = hotbar.create_bank("second").expect("create bank");
        hotbar
            .add_slot(first, first_stale, "beta-ctx")
            .expect("add slot");
        hotbar
            .add_slot(second, second_stale, "beta-ctx")
            .expect("the same label can appear in different banks");
        hotbar.save(&path).expect("save to disk");

        let index = cluster_index(&registry);
        let (loaded, error) = Hotbar::load(&path, |key| index.get(key).copied());
        assert!(error.is_none(), "{error:?}");
        assert!(loaded.banks[0].slots.is_empty(), "{loaded:?}");
        assert!(loaded.banks[1].slots.is_empty(), "{loaded:?}");
        assert_eq!(loaded.banks.len(), 2);
    }

    #[tokio::test]
    async fn cluster_index_keeps_stable_ids_for_duplicate_contexts() {
        let kubeconfig = kube::config::Kubeconfig::from_yaml(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: one
  cluster:
    server: http://127.0.0.1:6443
- name: two
  cluster:
    server: http://127.0.0.1:6444
contexts:
- name: duplicate
  context:
    cluster: one
- name: duplicate
  context:
    cluster: two
current-context: duplicate
"#,
        )
        .expect("parse kubeconfig");
        let registry = ClusterRegistry::from_kubeconfig(kubeconfig).await;
        let index = cluster_index(&registry);

        assert!(index.get("duplicate").is_none());
        for cluster in registry.clusters() {
            assert_eq!(index.get(&cluster.id().to_string()), Some(&cluster.id()));
        }
    }
}
