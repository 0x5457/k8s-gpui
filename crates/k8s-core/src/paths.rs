use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

pub const APP_DIR_NAME: &str = "k8s-gpui";

pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|path| path.join(APP_DIR_NAME))
}

pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|path| path.join(APP_DIR_NAME))
}

pub fn data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|path| path.join(APP_DIR_NAME))
}

#[cfg(target_os = "linux")]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir().map(|path| path.join(APP_DIR_NAME))
}

#[cfg(target_os = "macos")]
pub fn state_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|path| path.join(APP_DIR_NAME))
}

#[cfg(windows)]
pub fn state_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|path| path.join(APP_DIR_NAME))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn state_dir() -> Option<PathBuf> {
    dirs::state_dir().map(|path| path.join(APP_DIR_NAME))
}

pub fn download_dir() -> Option<PathBuf> {
    dirs::download_dir()
}

pub fn config_file(name: &str) -> Option<PathBuf> {
    config_dir().map(|path| path.join(name))
}

pub fn cache_file(name: &str) -> Option<PathBuf> {
    cache_dir().map(|path| path.join(name))
}

/// The last spec applied to each edited object (C2.5, `WRITE-OPS.md` §5).
pub fn history_file() -> Option<PathBuf> {
    config_file("history.json")
}

pub fn default_kubeconfig_paths() -> Vec<PathBuf> {
    kubeconfig_paths(std::env::var_os("KUBECONFIG"), home_dir())
}

/// `$KUBECONFIG` when it names something, otherwise `~/.kube/config`, and
/// nothing else.
///
/// **The rule is kubectl's rule, on purpose.** This used to sweep every
/// `*.yaml` and `*.yml` sitting in `~/.kube` and merge each one as a
/// kubeconfig, which bought two things nobody asked for: the app offered
/// contexts `kubectl config get-contexts` does not, so every "how do I reach
/// that cluster from a terminal" answer was app-specific; and any unrelated file
/// someone keeps in `~/.kube` — a downloaded manifest, a values file, a scratch
/// note — became a kubeconfig source and left a permanent "this kubeconfig could
/// not be read" warning naming a file they never pointed the app at. Splitting
/// kubeconfigs across files is a real thing to do and `$KUBECONFIG` is how it is
/// done, so the convenience is one environment variable away.
///
/// An empty list is the answer "this machine has no clusters", and it reaches
/// [`crate::cluster::ClusterError::NoKubeconfig`] rather than a read failure
/// that never happened.
pub(crate) fn kubeconfig_paths(value: Option<OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    if let Some(value) = value {
        let paths: Vec<_> = std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            return deduplicate(paths);
        }
    }

    home.map(|home| home.join(".kube").join("config"))
        .into_iter()
        .filter(|config| config.is_file())
        .collect()
}

fn deduplicate(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_file_paths_join_to_app_directories() {
        assert_eq!(APP_DIR_NAME, "k8s-gpui");

        if let Some(config) = config_dir() {
            assert_eq!(config_file("layout.json"), Some(config.join("layout.json")));
        }
        if let Some(cache) = cache_dir() {
            assert_eq!(
                cache_file("snapshot.json"),
                Some(cache.join("snapshot.json"))
            );
        }
    }

    /// A machine with no kubeconfig reports none, rather than reporting a file
    /// that is not there.
    #[test]
    fn a_machine_with_no_kubeconfig_has_none() {
        let root = tempfile::tempdir().expect("temp home");

        let found: Vec<PathBuf> = kubeconfig_paths(None, Some(root.path().to_path_buf()));
        assert!(found.is_empty(), "a home with no kubeconfig has none");
    }

    /// Only `~/.kube/config` is a kubeconfig, whatever else is in `~/.kube`.
    ///
    /// The app used to merge every `*.yaml` and `*.yml` beside it, a discovery
    /// rule kubectl does not have and a standing source of warnings about files
    /// the reader never named.
    #[test]
    fn only_the_default_config_is_read_beside_the_kube_directory() {
        let root = tempfile::tempdir().expect("temp home");
        let kube_dir = root.path().join(".kube");
        std::fs::create_dir_all(kube_dir.join("cache/nested")).expect("create kube directory");
        let config = kube_dir.join("config");
        std::fs::write(&config, "").expect("write config");
        std::fs::write(kube_dir.join("a.yml"), "").expect("write unrelated yaml");
        std::fs::write(kube_dir.join("z.yaml"), "").expect("write unrelated yaml");
        std::fs::write(kube_dir.join("cache/nested/ignored.yaml"), "").expect("write nested file");

        let paths = kubeconfig_paths(None, Some(root.path().to_path_buf()));

        assert_eq!(paths, [config]);
    }

    #[test]
    fn deduplicates_platform_separated_kubeconfig_paths() {
        let root = tempfile::tempdir().expect("temp directory");
        let path = root.path().join("config.yaml");
        std::fs::write(&path, "").expect("write config");
        let value = std::env::join_paths([&path, &path]).expect("join paths");

        let paths = kubeconfig_paths(Some(value), None);

        assert_eq!(paths, [path]);
    }
}
