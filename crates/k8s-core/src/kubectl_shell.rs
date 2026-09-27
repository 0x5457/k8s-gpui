//! Creates a temporary kubeconfig for one kubectl session.

use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

use kube::config::Kubeconfig;

use crate::atomic_file::create_private_dir_all;
use crate::cluster::Cluster;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("Failed to read kubeconfig: {0}. Check the kubeconfig file and try again.")]
    Kubeconfig(#[from] kube::config::KubeconfigError),

    #[error(
        "Context {context} was not found in kubeconfig. Select an existing context and try again."
    )]
    ContextNotFound { context: String },

    #[error(
        "Context {context} refers to missing cluster {cluster}. Fix the kubeconfig and try again."
    )]
    ClusterNotFound { context: String, cluster: String },

    #[error(
        "Failed to serialize the temporary kubeconfig: {0}. Check the file contents and try again."
    )]
    Serialize(#[from] serde_yaml_ng::Error),

    #[error(
        "Failed to write the temporary kubeconfig: {0}. Check the directory permissions and try again."
    )]
    Io(#[from] std::io::Error),
}

/// Owns a temporary kubeconfig and removes it on drop.
#[derive(Debug)]
pub struct SessionKubeconfig {
    path: PathBuf,
}

impl SessionKubeconfig {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SessionKubeconfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The session credential keeps the caller's own permissions.
///
/// The file is private (mode 0600 in a mode 0700 directory) and it only contains the
/// selected context, cluster, and user. It is **not** restricted to read-only: a caller
/// that hands the file to another program hands over write access to the cluster. A
/// session that crosses a process boundary must say so before the program runs.
pub const SESSION_CREDENTIAL_IS_UNRESTRICTED: bool = true;

/// Write the current context, cluster, and user to a private file under `dir`.
pub fn write_session_kubeconfig_from(
    cluster: &Cluster,
    source: &Kubeconfig,
    dir: &Path,
) -> Result<SessionKubeconfig, ShellError> {
    write_session_kubeconfig_from_inner(cluster, source, dir, NamespaceMode::Preserve)
}

pub fn write_session_kubeconfig_from_with_namespace(
    cluster: &Cluster,
    source: &Kubeconfig,
    dir: &Path,
    namespace: Option<&str>,
) -> Result<SessionKubeconfig, ShellError> {
    match namespace {
        Some(namespace) => {
            write_session_kubeconfig_from_inner(cluster, source, dir, NamespaceMode::Set(namespace))
        }
        None => write_session_kubeconfig_from(cluster, source, dir),
    }
}

pub fn write_session_kubeconfig_from_all_namespaces(
    cluster: &Cluster,
    source: &Kubeconfig,
    dir: &Path,
) -> Result<SessionKubeconfig, ShellError> {
    write_session_kubeconfig_from_inner(cluster, source, dir, NamespaceMode::All)
}

enum NamespaceMode<'a> {
    Preserve,
    Set(&'a str),
    All,
}

fn write_session_kubeconfig_from_inner(
    cluster: &Cluster,
    source: &Kubeconfig,
    dir: &Path,
    namespace: NamespaceMode<'_>,
) -> Result<SessionKubeconfig, ShellError> {
    let context_name = cluster.name();
    let named_context = source
        .contexts
        .iter()
        .find(|named| named.name == context_name)
        .ok_or_else(|| ShellError::ContextNotFound {
            context: context_name.to_string(),
        })?;
    let mut named_context = named_context.clone();
    match namespace {
        NamespaceMode::Set(namespace) => {
            let mut context = named_context.context.clone().unwrap_or_default();
            context.namespace = Some(namespace.to_owned());
            named_context.context = Some(context);
        }
        NamespaceMode::All => {
            let mut context = named_context.context.clone().unwrap_or_default();
            context.namespace = None;
            named_context.context = Some(context);
        }
        NamespaceMode::Preserve => {}
    }
    let context = named_context.context.clone().unwrap_or_default();
    let named_cluster = source
        .clusters
        .iter()
        .find(|named| named.name == context.cluster)
        .ok_or_else(|| ShellError::ClusterNotFound {
            context: context_name.to_string(),
            cluster: context.cluster.clone(),
        })?;
    let named_user = context.user.as_deref().and_then(|user| {
        source
            .auth_infos
            .iter()
            .find(|named| named.name == user)
            .cloned()
    });

    let minimal = Kubeconfig {
        clusters: vec![named_cluster.clone()],
        contexts: vec![named_context],
        auth_infos: named_user.into_iter().collect(),
        current_context: Some(context_name.to_string()),
        ..Kubeconfig::default()
    };
    let yaml = serde_yaml_ng::to_string(&minimal)?;
    let path = write_private(dir, yaml.as_bytes())?;
    Ok(SessionKubeconfig { path })
}

/// Environment for a kubectl shell. `KUBECONFIG` points to the session file.
pub fn shell_env(
    cluster: &Cluster,
    kubeconfig: &SessionKubeconfig,
    namespace: &str,
) -> Vec<(String, String)> {
    vec![
        (
            "KUBECONFIG".to_string(),
            kubeconfig.path().to_string_lossy().into_owned(),
        ),
        ("K8S_GPUI_CLUSTER".to_string(), cluster.name().to_string()),
        ("K8S_GPUI_NAMESPACE".to_string(), namespace.to_string()),
    ]
}

#[cfg(test)]
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_private(dir: &Path, bytes: &[u8]) -> Result<PathBuf, ShellError> {
    create_private_dir_all(dir)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("k8s-gpui-session-").suffix(".yaml");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut file = builder.tempfile_in(dir).map_err(ShellError::Io)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    let (file, path) = file.keep().map_err(|error| ShellError::Io(error.error))?;
    drop(file);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cluster::ClusterRegistry;

    const TWO_CONTEXTS: &str = r#"
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
    namespace: alpha-ns
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("k8s-gpui-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn write_kubeconfig(name: &str, contents: &str) -> PathBuf {
        let sequence = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "k8s-gpui-{}-{name}-{sequence}.yaml",
            std::process::id()
        ));
        std::fs::write(&path, contents).expect("write temp kubeconfig");
        path
    }

    async fn registry_from(contents: &str) -> ClusterRegistry {
        let path = write_kubeconfig("shell-source", contents);
        ClusterRegistry::load(&path).await.expect("load kubeconfig")
    }

    #[tokio::test]
    async fn minimal_kubeconfig_contains_only_current_context() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let dir = temp_dir("minimal");

        let session = write_session_kubeconfig_from(cluster, &source, &dir).expect("write");
        let written = Kubeconfig::read_from(session.path()).expect("read written");
        assert_eq!(written.contexts.len(), 1);
        assert_eq!(written.contexts[0].name, "alpha-ctx");
        assert_eq!(written.clusters.len(), 1);
        assert_eq!(written.clusters[0].name, "alpha");
        assert_eq!(written.auth_infos.len(), 1);
        assert_eq!(written.auth_infos[0].name, "alpha-user");
        assert_eq!(written.current_context.as_deref(), Some("alpha-ctx"));

        let path = session.path().to_path_buf();
        drop(session);
        assert!(!path.exists(), "cleanup removes the temporary kubeconfig");
    }

    #[tokio::test]
    async fn session_kubeconfig_preserves_context_namespace() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let session =
            write_session_kubeconfig_from(cluster, &source, &temp_dir("context-namespace"))
                .expect("write");
        let written = Kubeconfig::read_from(session.path()).expect("read written");

        assert_eq!(
            written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            Some("alpha-ns")
        );
    }

    #[tokio::test]
    async fn session_kubeconfig_rewrites_context_namespace() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let session = write_session_kubeconfig_from_with_namespace(
            cluster,
            &source,
            &temp_dir("rewritten-context-namespace"),
            Some("team-a"),
        )
        .expect("write");
        let written = Kubeconfig::read_from(session.path()).expect("read written");

        assert_eq!(
            written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            Some("team-a")
        );
    }

    #[tokio::test]
    async fn session_kubeconfig_all_namespaces_removes_context_namespace() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let session = write_session_kubeconfig_from_all_namespaces(
            cluster,
            &source,
            &temp_dir("all-namespaces"),
        )
        .expect("write");
        let written = Kubeconfig::read_from(session.path()).expect("read written");

        assert_eq!(
            written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.namespace.as_deref()),
            None
        );
    }

    #[tokio::test]
    async fn session_kubeconfig_is_private_and_removed_on_drop() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let dir = temp_dir("private");

        let session = write_session_kubeconfig_from(cluster, &source, &dir).expect("write");
        let path = session.path().to_path_buf();
        assert!(path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "temporary kubeconfig must use mode 0600");
            let dir_mode = std::fs::metadata(&dir)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                dir_mode, 0o700,
                "the new session directory must use mode 0700"
            );
        }

        drop(session);
        assert!(!path.exists(), "drop removes the temporary kubeconfig");
    }

    #[tokio::test]
    async fn missing_context_is_reported() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let mut source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        source.contexts.clear();
        let error = write_session_kubeconfig_from(cluster, &source, &temp_dir("missing"))
            .expect_err("context must exist");
        assert!(matches!(&error, ShellError::ContextNotFound { .. }));
        let display = error.to_string();
        assert!(display.contains("alpha-ctx"));
        assert!(!display.contains("\"alpha-ctx\""));
    }

    #[tokio::test]
    async fn missing_user_matches_gui_anonymous_auth() {
        let mut source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        source.auth_infos.clear();
        let registry = ClusterRegistry::from_kubeconfig(source.clone()).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let session =
            write_session_kubeconfig_from(cluster, &source, &temp_dir("no-user")).expect("write");
        let written = Kubeconfig::read_from(session.path()).expect("read written");
        assert!(written.auth_infos.is_empty());
        assert_eq!(
            written.contexts[0]
                .context
                .as_ref()
                .and_then(|context| context.user.as_deref()),
            Some("alpha-user")
        );
    }

    #[tokio::test]
    #[ignore = "Requires kind and kubectl: KUBECONFIG or ~/.kube/config (kind-k8s-gpui-dev)"]
    async fn kind_session_kubeconfig_serves_current_context() {
        if !crate::cluster::kubeconfig_present() {
            return;
        }
        let Ok(registry) = ClusterRegistry::load_default().await else {
            return;
        };
        // Use the dev context even when kubeconfig has other contexts.
        let Some(cluster) = registry
            .clusters()
            .iter()
            .find(|cluster| cluster.name() == "kind-k8s-gpui-dev")
        else {
            return;
        };
        let source_path = std::env::var_os("KUBECONFIG")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".kube/config"))
            });
        let before = source_path
            .as_ref()
            .and_then(|path| std::fs::read(path).ok());

        let dir = temp_dir("kind-session");
        let source = Kubeconfig::read().expect("read source kubeconfig");
        let session = write_session_kubeconfig_from(cluster, &source, &dir)
            .expect("write session kubeconfig");
        let path = session.path().to_path_buf();
        let env = shell_env(cluster, &session, "default");
        assert_eq!(env.len(), 3);

        let output = std::process::Command::new("kubectl")
            .args([
                "--kubeconfig",
                path.to_str().expect("utf8 path"),
                "get",
                "namespaces",
                "-o",
                "name",
            ])
            .output()
            .expect("kubectl is executable");
        assert!(
            output.status.success(),
            "the temporary kubeconfig connects to the cluster: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        eprintln!("[session-kubeconfig] kubectl --kubeconfig -> {stdout}");
        assert!(stdout.contains("namespace/default"), "output: {stdout}");

        // The global kubeconfig remains unchanged.
        if let (Some(source_path), Some(before)) = (source_path, before) {
            let after = std::fs::read(&source_path).expect("read the source kubeconfig again");
            assert_eq!(after, before, "the global kubeconfig must remain unchanged");
        }

        drop(session);
        assert!(
            !path.exists(),
            "the session removes the temporary kubeconfig"
        );
    }

    #[tokio::test]
    async fn session_kubeconfig_keeps_the_caller_credential() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let session =
            write_session_kubeconfig_from(cluster, &source, &temp_dir("scope")).expect("write");

        let written = Kubeconfig::read_from(session.path()).expect("read written");
        const {
            assert!(
                SESSION_CREDENTIAL_IS_UNRESTRICTED,
                "the session credential is the caller's own credential"
            );
        }
        assert!(
            written.contexts.len() == 1
                && written.clusters.len() == 1
                && written.auth_infos.len() == 1,
            "the session narrows the kubeconfig to one context: {written:?}"
        );
    }

    #[tokio::test]
    async fn session_kubeconfig_carries_the_caller_credential() {
        const CREDENTIAL: &str = "caller-token-value";
        const CREDENTIALS: &str = r#"
apiVersion: v1
kind: Config
clusters:
- name: alpha
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: alpha-ctx
  context:
    cluster: alpha
    user: alpha-user
users:
- name: alpha-user
  user:
    token: caller-token-value
current-context: alpha-ctx
"#;
        let registry = registry_from(CREDENTIALS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(CREDENTIALS).expect("parse");
        let session = write_session_kubeconfig_from(cluster, &source, &temp_dir("credential"))
            .expect("write");

        const {
            assert!(
                SESSION_CREDENTIAL_IS_UNRESTRICTED,
                "the session credential is the caller's own credential"
            );
        }
        let written = std::fs::read_to_string(session.path()).expect("read the session file");
        assert!(
            written.contains(CREDENTIAL),
            "the session hands over the caller's credential, so it is not read-only: {written}"
        );
    }

    #[tokio::test]
    async fn shell_env_exports_session_variables() {
        let registry = registry_from(TWO_CONTEXTS).await;
        let cluster = registry.clusters().first().expect("alpha-ctx");
        let source = Kubeconfig::from_yaml(TWO_CONTEXTS).expect("parse");
        let session =
            write_session_kubeconfig_from(cluster, &source, &temp_dir("env")).expect("write");

        let env = shell_env(cluster, &session, "alpha-ns");
        assert_eq!(
            env,
            vec![
                (
                    "KUBECONFIG".to_string(),
                    session.path().to_string_lossy().into_owned()
                ),
                ("K8S_GPUI_CLUSTER".to_string(), "alpha-ctx".to_string()),
                ("K8S_GPUI_NAMESPACE".to_string(), "alpha-ns".to_string()),
            ]
        );
    }
}
