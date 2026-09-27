use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io;
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

pub fn default_kubeconfig_paths() -> io::Result<Vec<PathBuf>> {
    kubeconfig_paths(std::env::var_os("KUBECONFIG"), home_dir())
}

pub(crate) fn kubeconfig_paths(
    value: Option<OsString>,
    home: Option<PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    if let Some(value) = value {
        let paths: Vec<_> = std::env::split_paths(&value)
            .filter(|path| !path.as_os_str().is_empty())
            .collect();
        if !paths.is_empty() {
            return Ok(deduplicate(paths));
        }
    }

    let Some(home) = home else {
        return Ok(Vec::new());
    };
    let kube_dir = home.join(".kube");
    let config = kube_dir.join("config");
    let mut paths = config
        .is_file()
        .then_some(config)
        .into_iter()
        .collect::<Vec<_>>();
    let entries = match fs::read_dir(&kube_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error),
    };
    let mut yaml_paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "yaml" || extension == "yml")
        {
            yaml_paths.push(path);
        }
    }
    yaml_paths.sort();
    paths.extend(yaml_paths);
    Ok(deduplicate(paths))
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
            assert_eq!(config_file("hotbar.json"), Some(config.join("hotbar.json")));
        }
        if let Some(cache) = cache_dir() {
            assert_eq!(
                cache_file("snapshot.json"),
                Some(cache.join("snapshot.json"))
            );
        }
    }

    #[test]
    fn discovers_immediate_kubeconfigs_in_stable_order_without_duplicates() {
        let root = tempfile::tempdir().expect("temp home");
        let kube_dir = root.path().join(".kube");
        std::fs::create_dir_all(kube_dir.join("cache/nested")).expect("create kube directory");
        let config = kube_dir.join("config");
        let yaml = kube_dir.join("z.yaml");
        let yml = kube_dir.join("a.yml");
        std::fs::write(&config, "").expect("write config");
        std::fs::write(&yaml, "").expect("write yaml");
        std::fs::write(&yml, "").expect("write yml");
        std::fs::write(kube_dir.join("notes.txt"), "").expect("write unrelated file");
        std::fs::write(kube_dir.join("cache.yaml.bak"), "").expect("write cache backup");
        std::fs::write(kube_dir.join("cache/nested/ignored.yaml"), "").expect("write nested file");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&config, kube_dir.join("config-alias.yaml"))
            .expect("link config alias");

        let paths =
            kubeconfig_paths(None, Some(root.path().to_path_buf())).expect("discover kubeconfigs");

        assert_eq!(paths, [config, yml, yaml]);
    }

    #[test]
    fn deduplicates_platform_separated_kubeconfig_paths() {
        let root = tempfile::tempdir().expect("temp directory");
        let path = root.path().join("config.yaml");
        std::fs::write(&path, "").expect("write config");
        let value = std::env::join_paths([&path, &path]).expect("join paths");

        let paths = kubeconfig_paths(Some(value), None).expect("resolve KUBECONFIG");

        assert_eq!(paths, [path]);
    }
}
