//! Persists hotbar banks and cluster slots in hotbar.json.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_file::{create_private_dir_all, write_atomic as write_atomic_bytes};
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

    /// Load from the default path.
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
        let json = serde_json::to_vec_pretty(&HotbarFile::from_hotbar(self))?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            create_private_dir_all(parent)?;
        }
        write_atomic_bytes(path, &json)?;
        Ok(())
    }

    /// Save to the default path.
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

    pub fn remove_slot(&mut self, bank: usize, slot: usize) -> Result<Slot, HotbarError> {
        let bank_ref = self
            .banks
            .get_mut(bank)
            .ok_or(HotbarError::NoSuchBank { index: bank })?;
        if slot >= bank_ref.slots.len() {
            return Err(HotbarError::NoSuchSlot { bank, slot });
        }
        Ok(bank_ref.slots.remove(slot))
    }

    /// Move a slot within a bank. Equal indexes are a no-op.
    pub fn move_slot(&mut self, bank: usize, from: usize, to: usize) -> Result<(), HotbarError> {
        let bank_ref = self
            .banks
            .get_mut(bank)
            .ok_or(HotbarError::NoSuchBank { index: bank })?;
        if from >= bank_ref.slots.len() {
            return Err(HotbarError::NoSuchSlot { bank, slot: from });
        }
        if to >= bank_ref.slots.len() {
            return Err(HotbarError::NoSuchSlot { bank, slot: to });
        }
        if from == to {
            return Ok(());
        }
        let slot = bank_ref.slots.remove(from);
        bank_ref.slots.insert(to, slot);
        Ok(())
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

        fn path(&self) -> &Path {
            &self.0
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

    fn labels(hotbar: &Hotbar, bank: usize) -> Vec<&str> {
        hotbar.banks[bank]
            .slots
            .iter()
            .map(|slot| slot.label.as_str())
            .collect()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("directory is readable")
            .map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
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
        // The stale slot is dropped rather than rebound to the same-named
        // context: a rebuilt cluster is a different cluster, and silently
        // adopting it would point the slot at an endpoint the user never chose.
        // The bank itself survives, so the user's other slots and the bank's
        // own name are not collateral damage of one dead entry.
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
        // A slot naming a cluster that left the registry is dropped, and it is
        // not an error: the hotbar is a convenience, and refusing to load it
        // used to take kubeconfig loading down with it.
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
    async fn a_bank_whose_slots_all_stale_loads_as_empty_rather_than_failing() {
        let dir = TempDir::new("resolve-all-stale");
        let path = dir.file();
        let raw = r#"{ "version": 1, "active": 0, "banks": [ {
            "name": "first",
            "slots": [ { "cluster_id": "gone-a", "label": "a" },
                       { "cluster_id": "gone-b", "label": "b" } ]
        } ] }"#;
        fs::write(&path, raw).expect("write file");

        let (loaded, error) = Hotbar::load(&path, |_| None);
        assert!(error.is_none(), "{error:?}");
        assert!(loaded.banks[0].slots.is_empty());
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
        // Neither stale slot is rebound to the other just because they share a
        // context label, and both banks survive the drop.
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

    #[test]
    fn load_clamps_out_of_range_active_bank() {
        let dir = TempDir::new("active-clamp");
        let path = dir.file();
        fs::write(
            &path,
            r#"{ "version": 1, "active": 5, "banks": [ { "name": "b", "slots": [] } ] }"#,
        )
        .expect("write file");
        let (loaded, error) = Hotbar::load(&path, id_resolver(&[]));
        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            loaded.active, 0,
            "an out-of-range active index uses the last bank"
        );
    }

    #[test]
    fn bank_slot_limit_is_twelve() {
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        for position in 0..MAX_SLOTS_PER_BANK {
            hotbar
                .add_slot(bank, id(&format!("ctx-{position}")), "x")
                .expect("12 slots are accepted");
        }
        let error = hotbar
            .add_slot(bank, id("overflow"), "x")
            .expect_err("the 13th slot reports an error");
        assert!(matches!(
            error,
            HotbarError::BankFull {
                max: MAX_SLOTS_PER_BANK,
                ..
            }
        ));
    }

    #[test]
    fn errors_use_display_values() {
        let full = HotbarError::BankFull {
            bank: "ops".to_owned(),
            max: MAX_SLOTS_PER_BANK,
        }
        .to_string();
        assert_eq!(
            full,
            "Bank ops is full. It has 12 slots. Remove a slot and try again."
        );

        let unresolved = HotbarError::UnresolvedSlot {
            cluster_id: id("missing").to_string(),
            label: "missing-ctx".to_owned(),
        }
        .to_string();
        assert!(!unresolved.contains('"'));
        assert!(unresolved.contains("cluster_id="));
        assert!(unresolved.contains("label=missing-ctx"));

        let duplicate = HotbarError::DuplicateCluster {
            bank: "ops".to_owned(),
        }
        .to_string();
        assert_eq!(
            duplicate,
            "The cluster is already in bank ops. Choose another bank."
        );
    }

    #[test]
    fn duplicate_cluster_rejected_within_bank_but_allowed_across_banks() {
        let alpha = id("alpha");
        let mut hotbar = Hotbar::default();
        let first = hotbar.create_bank("a").expect("create bank");
        let second = hotbar.create_bank("b").expect("create bank");
        hotbar.add_slot(first, alpha, "alpha").expect("add slot");

        let error = hotbar
            .add_slot(first, alpha, "dup")
            .expect_err("a duplicate in one bank reports an error");
        assert!(matches!(error, HotbarError::DuplicateCluster { .. }));
        hotbar
            .add_slot(second, alpha, "alpha")
            .expect("the same cluster can appear in another bank");
    }

    #[test]
    fn bank_operations_enforce_names_and_bounds() {
        let mut hotbar = Hotbar::default();
        assert!(matches!(
            hotbar.create_bank("  "),
            Err(HotbarError::EmptyBankName)
        ));

        let first = hotbar
            .create_bank("  prod  ")
            .expect("trim and create bank");
        assert_eq!(hotbar.banks[first].name, "prod");
        assert!(matches!(
            hotbar.create_bank("prod"),
            Err(HotbarError::DuplicateBank { .. })
        ));
        assert!(matches!(
            hotbar.rename_bank(9, "x"),
            Err(HotbarError::NoSuchBank { .. })
        ));
        assert!(matches!(
            hotbar.rename_bank(first, "  "),
            Err(HotbarError::EmptyBankName)
        ));

        let second = hotbar.create_bank("dev").expect("create bank");
        assert!(matches!(
            hotbar.rename_bank(second, "prod"),
            Err(HotbarError::DuplicateBank { .. })
        ));
        hotbar.rename_bank(first, "production").expect("rename");
        assert_eq!(hotbar.banks[first].name, "production");

        assert!(matches!(
            hotbar.remove_bank(9),
            Err(HotbarError::NoSuchBank { .. })
        ));
        assert!(matches!(
            hotbar.set_active(9),
            Err(HotbarError::NoSuchBank { .. })
        ));
        hotbar.set_active(1).expect("switch bank");
        assert_eq!(
            hotbar.active_bank().map(|bank| bank.name.as_str()),
            Some("dev")
        );
    }

    #[test]
    fn remove_bank_keeps_active_pointing_at_the_same_bank() {
        let mut hotbar = Hotbar::default();
        hotbar.create_bank("a").expect("create bank");
        hotbar.create_bank("b").expect("create bank");
        hotbar.create_bank("c").expect("create bank");
        hotbar.set_active(2).expect("switch to c");

        hotbar.remove_bank(0).expect("remove a");
        assert_eq!(hotbar.active, 1, "active still points to c after the shift");
        assert_eq!(
            hotbar.active_bank().map(|bank| bank.name.as_str()),
            Some("c")
        );

        hotbar.remove_bank(1).expect("remove c");
        assert_eq!(hotbar.active, 0);
        assert_eq!(
            hotbar.active_bank().map(|bank| bank.name.as_str()),
            Some("b")
        );

        hotbar.remove_bank(0).expect("remove the last bank");
        assert_eq!(hotbar.active, 0);
        assert!(hotbar.active_bank().is_none());
    }

    #[test]
    fn slot_operations_validate_bounds() {
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        assert!(matches!(
            hotbar.add_slot(9, id("a"), "a"),
            Err(HotbarError::NoSuchBank { .. })
        ));
        assert!(matches!(
            hotbar.remove_slot(bank, 0),
            Err(HotbarError::NoSuchSlot { .. })
        ));

        let alpha = id("alpha");
        hotbar.add_slot(bank, alpha, "alpha").expect("add slot");
        let removed = hotbar.remove_slot(bank, 0).expect("remove slot");
        assert_eq!(removed, Slot::new(alpha, "alpha"));
        assert!(matches!(
            hotbar.remove_slot(bank, 0),
            Err(HotbarError::NoSuchSlot { .. })
        ));
    }

    #[test]
    fn move_slot_reorders_deterministically() {
        let mut hotbar = Hotbar::default();
        let bank = hotbar.create_bank("default").expect("create bank");
        hotbar.add_slot(bank, id("alpha"), "a").expect("add slot");
        hotbar.add_slot(bank, id("beta"), "b").expect("add slot");
        hotbar.add_slot(bank, id("gamma"), "c").expect("add slot");

        hotbar.move_slot(bank, 0, 2).expect("move to the end");
        assert_eq!(labels(&hotbar, bank), ["b", "c", "a"]);
        hotbar.move_slot(bank, 2, 0).expect("move to the start");
        assert_eq!(labels(&hotbar, bank), ["a", "b", "c"]);
        hotbar
            .move_slot(bank, 1, 1)
            .expect("move to the same position is a no-op");
        assert_eq!(labels(&hotbar, bank), ["a", "b", "c"]);

        assert!(matches!(
            hotbar.move_slot(bank, 3, 0),
            Err(HotbarError::NoSuchSlot { slot: 3, .. })
        ));
        assert!(matches!(
            hotbar.move_slot(bank, 0, 3),
            Err(HotbarError::NoSuchSlot { slot: 3, .. })
        ));
        assert!(matches!(
            hotbar.move_slot(9, 0, 0),
            Err(HotbarError::NoSuchBank { .. })
        ));
    }

    #[test]
    fn atomic_save_leaves_no_temp_files() {
        let dir = TempDir::new("atomic");
        let path = dir.file();
        let hotbar = hotbar_with(&[id("alpha")]);
        hotbar.save(&path).expect("save to disk");

        assert_eq!(entries(dir.path()), [FILE_NAME]);

        let (loaded, error) = Hotbar::load(&path, id_resolver(&[id("alpha")]));
        assert!(error.is_none());
        assert_eq!(loaded, hotbar);
    }

    #[test]
    fn concurrent_saves_leave_a_complete_file() {
        let dir = TempDir::new("concurrent");
        let path = dir.file();
        let ids: Vec<ClusterId> = (0..8)
            .map(|position| id(&format!("ctx-{position}")))
            .collect();
        let hotbars: Vec<Hotbar> = ids.iter().map(|cluster| hotbar_with(&[*cluster])).collect();

        std::thread::scope(|scope| {
            for hotbar in &hotbars {
                let path = path.clone();
                scope.spawn(move || hotbar.save(&path).expect("concurrent save to disk"));
            }
        });

        let (loaded, error) = Hotbar::load(&path, id_resolver(&ids));
        assert!(error.is_none(), "concurrent writes leave valid JSON");
        assert!(
            hotbars.contains(&loaded),
            "the content is one complete write"
        );
        assert_eq!(
            entries(dir.path()),
            [FILE_NAME],
            "no temporary files remain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_private_dirs_and_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("permissions");
        let root = dir.path().join("nested").join("config");
        let path = root.join(FILE_NAME);
        hotbar_with(&[id("alpha")])
            .save(&path)
            .expect("save to disk");

        let dir_mode = fs::metadata(&root).expect("directory").permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = fs::metadata(&path).expect("file").permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[test]
    fn default_path_points_into_config_dir() {
        let path = default_path().expect("default configuration path");
        assert!(path.ends_with(Path::new("k8s-gpui").join(FILE_NAME)));
    }

    #[tokio::test]
    async fn cluster_index_resolves_only_stable_ids() {
        let registry = test_registry().await;
        assert!(!registry.clusters().is_empty());

        let index = cluster_index(&registry);
        for cluster in registry.clusters() {
            assert_eq!(
                index.get(&cluster.id().to_string()),
                Some(&cluster.id()),
                "the hexadecimal stable ID resolves to ClusterId"
            );
            assert_eq!(
                index.get(cluster.name()),
                None,
                "the context name does not resolve a stable ID"
            );
        }

        let dir = TempDir::new("cluster-index");
        let cluster = registry.clusters()[0].id();
        let hotbar = hotbar_with(&[cluster]);
        hotbar.save(dir.file()).expect("save to disk");
        let (loaded, error) = Hotbar::load(dir.file(), |key| index.get(key).copied());
        assert!(error.is_none());
        assert_eq!(loaded, hotbar, "the UI path round-trips completely");
    }
}
