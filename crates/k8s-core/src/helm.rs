//! Runs the Helm CLI and parses its JSON output.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;

/// Executable name searched in PATH.
pub(crate) const HELM_BINARY: &str = "helm";

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub enum HelmError {
    /// The UI hides Helm when the executable is missing.
    #[error("Helm was not found. Check the executable path or PATH, then try again.")]
    NotInstalled,

    #[error("Failed to run Helm: {0}. Check the executable and permissions, then try again.")]
    Io(#[from] std::io::Error),

    #[error(
        "Helm detection timed out after 3 seconds. Check that Helm starts from PATH, then try again."
    )]
    Timeout,

    /// The raw stderr text is preserved for diagnosis.
    #[error(
        "Helm exited {}: {stderr}. Review the Helm output and try again.",
        match .code {
            Some(code) => format!("with code {code}"),
            None => "without an exit code".to_owned(),
        }
    )]
    Exit { code: Option<i32>, stderr: String },

    #[error(
        "Failed to parse Helm output: {source}. Check the Helm version and output format, then try again."
    )]
    Parse {
        #[source]
        source: serde_json::Error,
        stdout: String,
    },

    #[error("Invalid Helm argument {argument}: {reason}. Correct the argument and try again.")]
    InvalidArgument {
        argument: String,
        reason: &'static str,
    },
}

impl HelmError {
    /// Used by the UI to decide whether to show Helm.
    pub fn is_not_installed(&self) -> bool {
        matches!(self, Self::NotInstalled)
    }
}

/// Helm release status. Unknown values map to [`ReleaseStatus::Unknown`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReleaseStatus {
    #[default]
    Unknown,
    Deployed,
    Uninstalled,
    Superseded,
    Failed,
    Uninstalling,
    PendingInstall,
    PendingUpgrade,
    PendingRollback,
}

impl ReleaseStatus {
    pub fn from_wire(value: &str) -> Self {
        match value {
            "deployed" => Self::Deployed,
            "uninstalled" => Self::Uninstalled,
            "superseded" => Self::Superseded,
            "failed" => Self::Failed,
            "uninstalling" => Self::Uninstalling,
            "pending-install" => Self::PendingInstall,
            "pending-upgrade" => Self::PendingUpgrade,
            "pending-rollback" => Self::PendingRollback,
            _ => Self::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Deployed => "deployed",
            Self::Uninstalled => "uninstalled",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
            Self::Uninstalling => "uninstalling",
            Self::PendingInstall => "pending-install",
            Self::PendingUpgrade => "pending-upgrade",
            Self::PendingRollback => "pending-rollback",
        }
    }
}

impl<'de> Deserialize<'de> for ReleaseStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&value))
    }
}

/// One row from `helm list -o json`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Release {
    pub name: String,
    pub namespace: String,
    /// Helm serializes the revision as a string.
    pub revision: String,
    pub updated: String,
    pub status: ReleaseStatus,
    /// Chart name and version joined as `chart-version`.
    pub chart: String,
    #[serde(default)]
    pub app_version: String,
}

/// Release object from `helm status/upgrade -o json`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ReleaseDetail {
    pub name: String,
    pub namespace: String,
    /// Current revision from `status`.
    #[serde(default)]
    pub version: i64,
    #[serde(default)]
    pub info: ReleaseInfo,
    #[serde(default)]
    pub chart: ChartRef,
}

/// One row from `helm history -o json`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ReleaseRevision {
    pub revision: i64,
    #[serde(default)]
    pub updated: String,
    pub status: ReleaseStatus,
    /// Chart name and version joined as `chart-version`.
    pub chart: String,
    #[serde(default)]
    pub app_version: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ReleaseInfo {
    #[serde(default)]
    pub status: ReleaseStatus,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub first_deployed: String,
    #[serde(default)]
    pub last_deployed: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ChartRef {
    #[serde(default)]
    pub metadata: ChartMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct ChartMetadata {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, rename = "appVersion")]
    pub app_version: String,
}

/// Detected Helm client.
#[derive(Clone, Debug)]
pub struct Helm {
    binary: PathBuf,
    kube_context: Option<String>,
    kubeconfig_sources: Vec<PathBuf>,
}

impl Default for Helm {
    fn default() -> Self {
        Self::with_binary(HELM_BINARY)
    }
}

impl Helm {
    /// Set the executable path.
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            kube_context: None,
            kubeconfig_sources: Vec::new(),
        }
    }

    /// Detect `helm` in PATH.
    pub async fn detect() -> Result<Self, HelmError> {
        Self::default().probe().await
    }

    /// Run `helm version --short` to confirm that the client works.
    pub async fn probe(self) -> Result<Self, HelmError> {
        self.probe_with_timeout(PROBE_TIMEOUT).await
    }

    async fn probe_with_timeout(self, timeout: Duration) -> Result<Self, HelmError> {
        let output = match tokio::time::timeout(
            timeout,
            self.command(["version", "--short"]).output(),
        )
        .await
        {
            Ok(output) => output,
            Err(_) => return Err(HelmError::Timeout),
        };
        match output {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(HelmError::NotInstalled)
            }
            Err(error) => Err(error.into()),
            Ok(output) if output.status.success() => Ok(self),
            Ok(output) => Err(exit_error(output)),
        }
    }

    /// Bind a kube context without changing the global kubeconfig.
    pub fn with_kube_context(mut self, context: impl Into<String>) -> Self {
        self.kube_context = Some(context.into());
        self
    }

    pub fn with_kubeconfig_sources<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        self.kubeconfig_sources = paths.into_iter().map(Into::into).collect();
        self
    }

    /// List releases in all namespaces or one namespace.
    pub async fn list_releases(&self, namespace: Option<&str>) -> Result<Vec<Release>, HelmError> {
        let mut args = self.subcommand("list");
        match namespace {
            Some(namespace) => {
                validate_argument("namespace", namespace)?;
                args.push("-n".to_owned());
                args.push(namespace.to_owned());
            }
            None => args.push("-A".to_owned()),
        }
        args.push("-o".to_owned());
        args.push("json".to_owned());
        self.json(&args).await
    }

    pub async fn status(&self, name: &str, namespace: &str) -> Result<ReleaseDetail, HelmError> {
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("status");
        args.push(name.to_owned());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        args.push("-o".to_owned());
        args.push("json".to_owned());
        self.json(&args).await
    }

    pub async fn history(
        &self,
        name: &str,
        namespace: &str,
    ) -> Result<Vec<ReleaseRevision>, HelmError> {
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("history");
        args.push(name.to_owned());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        args.push("-o".to_owned());
        args.push("json".to_owned());
        self.json(&args).await
    }

    /// Read-only release values as YAML.
    ///
    /// `all` adds the chart defaults and the computed values, so the user can see
    /// where a value came from instead of only what was set.
    pub async fn values(
        &self,
        name: &str,
        namespace: &str,
        all: bool,
    ) -> Result<String, HelmError> {
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("get");
        args.push("values".to_owned());
        args.push(name.to_owned());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        args.push("-o".to_owned());
        args.push("yaml".to_owned());
        if all {
            args.push("--all".to_owned());
        }
        self.run(&args).await
    }

    pub async fn uninstall(&self, name: &str, namespace: &str) -> Result<(), HelmError> {
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("uninstall");
        args.push(name.to_owned());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        self.run(&args).await.map(|_| ())
    }

    pub async fn rollback(
        &self,
        name: &str,
        namespace: &str,
        revision: u32,
    ) -> Result<(), HelmError> {
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("rollback");
        args.push(name.to_owned());
        args.push(revision.to_string());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        self.run(&args).await.map(|_| ())
    }

    pub async fn upgrade(
        &self,
        chart: &str,
        name: &str,
        namespace: &str,
    ) -> Result<ReleaseDetail, HelmError> {
        validate_argument("chart", chart)?;
        if chart.chars().any(char::is_control) {
            return Err(HelmError::InvalidArgument {
                argument: format!("chart={chart}"),
                reason: "Control characters are not allowed.",
            });
        }
        validate_argument("name", name)?;
        validate_argument("namespace", namespace)?;
        let mut args = self.subcommand("upgrade");
        args.push(name.to_owned());
        args.push(chart.to_owned());
        args.push("-n".to_owned());
        args.push(namespace.to_owned());
        args.push("--reuse-values".to_owned());
        args.push("--wait=false".to_owned());
        args.push("-o".to_owned());
        args.push("json".to_owned());
        self.json(&args).await
    }

    /// Build the common subcommand and cluster flags.
    fn subcommand(&self, subcommand: &str) -> Vec<String> {
        let mut args = Vec::with_capacity(6);
        if let Some(context) = &self.kube_context {
            args.push("--kube-context".to_owned());
            args.push(context.clone());
        }
        args.push(subcommand.to_owned());
        args
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(&self.binary);
        command.args(args);
        command.env_remove("KUBECONFIG");
        if !self.kubeconfig_sources.is_empty()
            && let Ok(kubeconfig) = std::env::join_paths(&self.kubeconfig_sources)
        {
            command.env("KUBECONFIG", kubeconfig);
        }
        command.kill_on_drop(true);
        command
    }

    async fn run(&self, args: &[String]) -> Result<String, HelmError> {
        let output = self.command(args).output().await;
        match output {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(HelmError::NotInstalled)
            }
            Err(error) => Err(error.into()),
            Ok(output) if output.status.success() => {
                Ok(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            Ok(output) => Err(exit_error(output)),
        }
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, args: &[String]) -> Result<T, HelmError> {
        let stdout = self.run(args).await?;
        serde_json::from_str(&stdout).map_err(|source| HelmError::Parse { source, stdout })
    }
}

/// Reject empty values and leading `-` before placing arguments in argv.
fn validate_argument(label: &'static str, value: &str) -> Result<(), HelmError> {
    let argument = format!("{label}={value}");
    if value.trim().is_empty() {
        return Err(HelmError::InvalidArgument {
            argument,
            reason: "Value must not be empty.",
        });
    }
    if value.starts_with('-') {
        return Err(HelmError::InvalidArgument {
            argument,
            reason: "Value must not start with '-'.",
        });
    }
    Ok(())
}

fn exit_error(output: std::process::Output) -> HelmError {
    HelmError::Exit {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    const LIST_JSON: &str = r#"[
      {
        "name": "ingress-nginx",
        "namespace": "ingress",
        "revision": "4",
        "updated": "2026-09-23 10:00:00.000000000 +0800 CST",
        "status": "deployed",
        "chart": "ingress-nginx-4.11.3",
        "app_version": "1.11.3"
      },
      {
        "name": "metrics-server",
        "namespace": "kube-system",
        "revision": "1",
        "updated": "2026-09-23 09:00:00.000000000 +0800 CST",
        "status": "pending-upgrade",
        "chart": "metrics-server-3.12.2",
        "app_version": "0.7.2"
      }
    ]"#;

    /// Captured from Helm 3.17 `status -o json`.
    /// It has no chart field.
    const STATUS_JSON: &str = r#"{
      "name": "ingress-nginx",
      "namespace": "ingress",
      "version": 4,
      "info": {
        "first_deployed": "2026-09-22T10:00:00Z",
        "last_deployed": "2026-09-23T10:00:00Z",
        "deleted": "",
        "description": "Upgrade complete",
        "status": "deployed",
        "notes": "The ingress-nginx controller has been installed."
      },
      "manifest": "---"
    }"#;

    /// Captured from Helm 3.17 `history -o json`.
    /// The chart value is a `name-version` string.
    const HISTORY_JSON: &str = r#"[
      {
        "revision": 3,
        "updated": "2026-09-23T09:00:00Z",
        "status": "superseded",
        "chart": "ingress-nginx-4.11.2",
        "app_version": "1.11.2",
        "description": "Upgrade complete"
      },
      {
        "revision": 4,
        "updated": "2026-09-23T10:00:00Z",
        "status": "deployed",
        "chart": "ingress-nginx-4.11.3",
        "app_version": "1.11.3",
        "description": "Upgrade complete"
      }
    ]"#;

    const UPGRADE_JSON: &str = r#"{
      "name": "demo",
      "namespace": "default",
      "version": 2,
      "info": { "status": "deployed", "description": "Upgrade complete" },
      "chart": { "metadata": { "name": "demo", "version": "0.1.0", "appVersion": "1.0.0" } }
    }"#;

    const VALUES_YAML: &str = "replicaCount: 2\nimage:\n  tag: 1.2.3\n";

    const FAKE_HELM: &str = r#"#!/bin/sh
dir=$(dirname "$0")
log="$dir/args.log"
printf '%s\n' "$*" >> "$log"
printf '%s' "${KUBECONFIG-}" > "$dir/kubeconfig.env"

if [ -f "$dir/sleep" ]; then
  sleep "$(cat "$dir/sleep")"
fi

if [ -f "$dir/exit_code" ]; then
  if [ -f "$dir/stderr.txt" ]; then cat "$dir/stderr.txt" >&2; fi
  exit "$(cat "$dir/exit_code")"
fi

cmd=""
for arg in "$@"; do
  case "$arg" in
    version|list|status|history|upgrade|uninstall|rollback|get) cmd="$arg"; break ;;
  esac
done

case "$cmd" in
  version) echo "v3.17.3+fake" ;;
  list) cat "$dir/list.json" ;;
  status) cat "$dir/status.json" ;;
  history) cat "$dir/history.json" ;;
  get) cat "$dir/values.yaml" ;;
  upgrade)
    cat "$dir/upgrade.json"
    ;;
  uninstall|rollback) : ;;
  *)
    echo "unknown command: $cmd" >&2
    exit 2
    ;;
esac
"#;

    struct FakeHelm {
        dir: PathBuf,
        binary: PathBuf,
    }

    /// Write the script once. Each test uses a symlink so parallel tests do not share one file.
    fn shared_script() -> &'static PathBuf {
        static SCRIPT: std::sync::LazyLock<PathBuf> = std::sync::LazyLock::new(|| {
            let dir = std::env::temp_dir()
                .join(format!("k8s-gpui-fake-helm-shared-{}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create shared fake Helm directory");
            let staged = dir.join("helm.staged");
            std::fs::write(&staged, FAKE_HELM).expect("write fake Helm script");
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                .expect("set executable permission");
            let script = dir.join("helm");
            std::fs::rename(&staged, &script).expect("place fake Helm script");
            script
        });
        &SCRIPT
    }

    impl FakeHelm {
        fn new() -> Self {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("k8s-gpui-fake-helm-{}-{id}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("create fake Helm directory");
            let binary = dir.join("helm");
            std::os::unix::fs::symlink(shared_script(), &binary).expect("link fake Helm script");
            std::fs::write(dir.join("list.json"), LIST_JSON).expect("write list fixture");
            std::fs::write(dir.join("status.json"), STATUS_JSON).expect("write status fixture");
            std::fs::write(dir.join("history.json"), HISTORY_JSON).expect("write history fixture");
            std::fs::write(dir.join("upgrade.json"), UPGRADE_JSON).expect("write upgrade fixture");
            std::fs::write(dir.join("values.yaml"), VALUES_YAML).expect("write values fixture");
            Self { dir, binary }
        }

        fn helm(&self) -> Helm {
            Helm::with_binary(&self.binary)
        }

        fn log(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("args.log"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn kubeconfig_env(&self) -> String {
            std::fs::read_to_string(self.dir.join("kubeconfig.env")).unwrap_or_default()
        }

        fn fail_with(&self, code: i32, stderr: &str) {
            std::fs::write(self.dir.join("exit_code"), code.to_string()).expect("write exit code");
            std::fs::write(self.dir.join("stderr.txt"), stderr).expect("write stderr");
        }
    }

    impl Drop for FakeHelm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[tokio::test]
    async fn detect_finds_helm_in_path() {
        let fake = FakeHelm::new();
        let helm = fake.helm().probe().await.expect("fake script is detected");
        assert_eq!(helm.binary, fake.binary);
        assert_eq!(fake.log(), ["version --short"]);
    }

    #[tokio::test]
    async fn probe_times_out_with_actionable_error() {
        let fake = FakeHelm::new();
        std::fs::write(fake.dir.join("sleep"), "10").expect("write delay");
        let error = fake
            .helm()
            .probe_with_timeout(std::time::Duration::from_millis(20))
            .await
            .expect_err("probe must time out");
        assert!(matches!(&error, HelmError::Timeout));
        assert!(error.to_string().contains("3 seconds"), "{error}");
        assert!(error.to_string().contains("PATH"), "{error}");
    }

    #[tokio::test]
    async fn detect_reports_missing_binary() {
        let error = Helm::with_binary("/nonexistent/k8s-gpui/helm")
            .probe()
            .await
            .expect_err("a missing path reports NotInstalled");
        assert!(matches!(error, HelmError::NotInstalled));
        assert!(error.is_not_installed());
    }

    #[tokio::test]
    async fn list_all_namespaces_parses_json() {
        let fake = FakeHelm::new();
        let releases = fake.helm().list_releases(None).await.expect("list parses");
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].name, "ingress-nginx");
        assert_eq!(releases[0].namespace, "ingress");
        assert_eq!(releases[0].revision, "4");
        assert_eq!(releases[0].status, ReleaseStatus::Deployed);
        assert_eq!(releases[0].chart, "ingress-nginx-4.11.3");
        assert_eq!(releases[0].app_version, "1.11.3");
        assert_eq!(releases[1].status, ReleaseStatus::PendingUpgrade);
        assert_eq!(fake.log(), ["list -A -o json"]);
    }

    #[tokio::test]
    async fn list_namespace_scoped_passes_namespace_flag() {
        let fake = FakeHelm::new();
        let releases = fake
            .helm()
            .list_releases(Some("kube-system"))
            .await
            .expect("list parses");
        assert_eq!(
            releases.len(),
            2,
            "the fake script ignores namespace and only checks arguments"
        );
        assert_eq!(fake.log(), ["list -n kube-system -o json"]);
    }

    #[tokio::test]
    async fn status_parses_info_without_chart() {
        let fake = FakeHelm::new();
        let detail = fake
            .helm()
            .status("ingress-nginx", "ingress")
            .await
            .expect("status parses");
        assert_eq!(detail.name, "ingress-nginx");
        assert_eq!(detail.namespace, "ingress");
        assert_eq!(detail.version, 4);
        assert_eq!(detail.info.status, ReleaseStatus::Deployed);
        assert_eq!(detail.info.description, "Upgrade complete");
        assert_eq!(
            detail.chart.metadata.name, "",
            "real Helm status has no chart field"
        );
        assert_eq!(fake.log(), ["status ingress-nginx -n ingress -o json"]);
    }

    #[tokio::test]
    async fn history_parses_flat_revisions_in_order() {
        let fake = FakeHelm::new();
        let history = fake
            .helm()
            .history("ingress-nginx", "ingress")
            .await
            .expect("history parses");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].revision, 3);
        assert_eq!(history[0].status, ReleaseStatus::Superseded);
        assert_eq!(history[0].chart, "ingress-nginx-4.11.2");
        assert_eq!(history[0].app_version, "1.11.2");
        assert_eq!(history[0].description, "Upgrade complete");
        assert_eq!(history[1].revision, 4);
        assert_eq!(history[1].status, ReleaseStatus::Deployed);
        assert_eq!(fake.log(), ["history ingress-nginx -n ingress -o json"]);
    }

    #[test]
    fn unknown_status_maps_to_unknown_without_failing() {
        assert_eq!(
            ReleaseStatus::from_wire("brand-new"),
            ReleaseStatus::Unknown
        );
        assert_eq!(ReleaseStatus::from_wire(""), ReleaseStatus::Unknown);
        assert_eq!(
            ReleaseStatus::from_wire("deployed"),
            ReleaseStatus::Deployed
        );
    }

    #[tokio::test]
    async fn nonzero_exit_carries_stderr_verbatim() {
        let fake = FakeHelm::new();
        fake.fail_with(1, "Error: release: not found\n");
        let error = fake
            .helm()
            .status("ghost", "default")
            .await
            .expect_err("a non-zero exit reports an error");
        match error {
            HelmError::Exit { code, stderr } => {
                assert_eq!(code, Some(1));
                assert_eq!(stderr, "Error: release: not found");
            }
            other => panic!("expected Exit, got {other:?}"),
        }
    }

    #[test]
    fn exit_error_formats_optional_code_for_users() {
        let with_code = HelmError::Exit {
            code: Some(1),
            stderr: "Error: release not found".to_owned(),
        }
        .to_string();
        assert_eq!(
            with_code,
            "Helm exited with code 1: Error: release not found. Review the Helm output and try again."
        );

        let without_code = HelmError::Exit {
            code: None,
            stderr: "terminated".to_owned(),
        }
        .to_string();
        assert_eq!(
            without_code,
            "Helm exited without an exit code: terminated. Review the Helm output and try again."
        );

        let invalid_argument = HelmError::InvalidArgument {
            argument: "name=--kubeconfig".to_owned(),
            reason: "Value must not start with '-'.",
        }
        .to_string();
        assert!(invalid_argument.contains("name=--kubeconfig"));
        assert!(!invalid_argument.contains("\"name=--kubeconfig\""));
    }

    #[tokio::test]
    async fn invalid_json_reports_stdout() {
        let fake = FakeHelm::new();
        std::fs::write(fake.dir.join("list.json"), "{not json").expect("write invalid fixture");
        let error = fake
            .helm()
            .list_releases(None)
            .await
            .expect_err("invalid JSON reports an error");
        match error {
            HelmError::Parse { stdout, .. } => assert_eq!(stdout, "{not json"),
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn uninstall_and_rollback_pass_expected_args() {
        let fake = FakeHelm::new();
        let helm = fake.helm();
        helm.uninstall("demo", "default").await.expect("uninstall");
        helm.rollback("demo", "default", 2).await.expect("rollback");
        assert_eq!(
            fake.log(),
            ["uninstall demo -n default", "rollback demo 2 -n default"]
        );
    }

    #[tokio::test]
    async fn upgrade_reuses_values_and_parses_release() {
        let fake = FakeHelm::new();
        let detail = fake
            .helm()
            .upgrade("bitnami/nginx", "demo", "default")
            .await
            .expect("upgrade parses");
        assert_eq!(detail.version, 2);
        assert_eq!(detail.info.status, ReleaseStatus::Deployed);
        assert_eq!(
            detail.chart.metadata.name, "demo",
            "upgrade output includes chart"
        );
        assert_eq!(detail.chart.metadata.version, "0.1.0");
        assert_eq!(
            fake.log(),
            ["upgrade demo bitnami/nginx -n default --reuse-values --wait=false -o json"]
        );
    }

    /// The values view is read-only: `helm get values` never changes a release.
    #[tokio::test]
    async fn values_reads_user_supplied_values_and_never_mutates() {
        let fake = FakeHelm::new();
        let values = fake
            .helm()
            .values("demo", "default", false)
            .await
            .expect("values parse");
        assert_eq!(values, VALUES_YAML);
        assert_eq!(fake.log(), ["get values demo -n default -o yaml"]);

        let all = fake
            .helm()
            .with_kube_context("kind-dev")
            .values("demo", "default", true)
            .await
            .expect("values parse");
        assert_eq!(all, VALUES_YAML);
        assert_eq!(
            fake.log().last().map(String::as_str),
            Some("--kube-context kind-dev get values demo -n default -o yaml --all")
        );
    }

    #[tokio::test]
    async fn cluster_binding_flags_come_before_subcommand() {
        let fake = FakeHelm::new();
        let first = PathBuf::from("/tmp/kubeconfig-one.yaml");
        let second = PathBuf::from("/tmp/kubeconfig-two.yaml");
        let before = std::env::var_os("KUBECONFIG");
        fake.helm()
            .with_kube_context("kind-k8s-gpui-dev")
            .with_kubeconfig_sources([first.clone(), second.clone()])
            .list_releases(None)
            .await
            .expect("list parses");
        assert_eq!(
            fake.log(),
            ["--kube-context kind-k8s-gpui-dev list -A -o json"]
        );
        assert_eq!(
            fake.kubeconfig_env(),
            std::env::join_paths([&first, &second])
                .expect("join paths")
                .to_string_lossy()
        );
        assert_eq!(std::env::var_os("KUBECONFIG"), before);

        fake.helm()
            .with_kube_context("kind-k8s-gpui-dev")
            .list_releases(None)
            .await
            .expect("list parses");
        assert!(
            !fake
                .log()
                .last()
                .is_some_and(|args| args.contains("--kubeconfig"))
        );
        assert_eq!(fake.kubeconfig_env(), "");
        assert_eq!(std::env::var_os("KUBECONFIG"), before);
    }

    #[tokio::test]
    async fn flag_like_arguments_are_rejected_before_spawning() {
        let fake = FakeHelm::new();
        let error = fake
            .helm()
            .status("--kubeconfig=/evil", "default")
            .await
            .expect_err("reject a leading '-'");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .list_releases(Some(""))
            .await
            .expect_err("reject an empty namespace");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .values("--kubeconfig=/evil", "default", false)
            .await
            .expect_err("reject a leading '-'");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .values("demo", "", true)
            .await
            .expect_err("reject an empty namespace");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .upgrade("", "demo", "default")
            .await
            .expect_err("reject an empty chart");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .upgrade("--post-renderer", "demo", "default")
            .await
            .expect_err("reject a flag-like chart");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .upgrade("   ", "demo", "default")
            .await
            .expect_err("reject a blank chart");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        let error = fake
            .helm()
            .upgrade("bitnami/nginx\nnext", "demo", "default")
            .await
            .expect_err("chart with control characters must be rejected");
        assert!(matches!(error, HelmError::InvalidArgument { .. }));
        assert!(
            fake.log().is_empty(),
            "rejected arguments must not reach the child process"
        );
    }

    #[test]
    fn missing_optional_json_fields_default_to_empty() {
        let release: Release = serde_json::from_str(
            r#"{"name":"a","namespace":"b","revision":"1","updated":"","status":"weird","chart":"c-1"}"#,
        )
        .expect("optional fields can be absent");
        assert_eq!(release.app_version, "");
        assert_eq!(release.status, ReleaseStatus::Unknown);
    }

    #[tokio::test]
    async fn not_installed_is_reported_when_binary_disappears() {
        let fake = FakeHelm::new();
        std::fs::remove_file(&fake.binary).expect("remove fake script");
        let error = fake
            .helm()
            .list_releases(None)
            .await
            .expect_err("a missing script reports NotInstalled");
        assert!(error.is_not_installed());
    }

    #[tokio::test]
    #[ignore = "Requires Helm and a cluster: helm is in PATH and kubeconfig is available"]
    async fn real_helm_lists_parses_status_and_history() {
        let Ok(helm) = Helm::detect().await else {
            return;
        };
        // Do not assume that the cluster has releases.
        let releases = helm
            .list_releases(None)
            .await
            .expect("real Helm list succeeds");
        eprintln!("Releases: {}", releases.len());
        if let Some(release) = releases.first() {
            let status = helm
                .status(&release.name, &release.namespace)
                .await
                .expect("status parses");
            let history = helm
                .history(&release.name, &release.namespace)
                .await
                .expect("history parses");
            eprintln!(
                "{} v{} {:?} ({} revisions)",
                status.name,
                status.version,
                status.info.status,
                history.len()
            );
            assert_eq!(status.name, release.name);
            assert!(history.iter().all(|revision| revision.revision > 0));
            assert!(history.iter().all(|revision| !revision.chart.is_empty()));
        }
    }
}
