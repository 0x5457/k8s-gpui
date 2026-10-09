use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::atomic_file::write_json_atomic;
use crate::paths;

pub(crate) const MANIFEST_SCHEMA: u32 = 1;
#[cfg(test)]
pub(crate) const DEFAULT_CHANNEL: &str = "stable";
pub const UPDATE_TARGET_OS: &str = "linux";
/// The architecture this build downloads updates for. The release workflow
/// publishes one updater manifest per Linux architecture, so the target comes
/// from the compilation target instead of a fixed string.
pub const UPDATE_TARGET_ARCH: &str = std::env::consts::ARCH;
pub(crate) const UPDATE_STATE_FILE: &str = "update-state.json";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading,
    Ready,
    Restarting,
    Failed,
    Unsupported,
}

impl UpdatePhase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle",
            Self::Checking => "Checking…",
            Self::UpToDate => "Up to date",
            Self::Downloading => "Downloading",
            Self::Ready => "Ready",
            Self::Restarting => "Restarting",
            Self::Failed => "Failed",
            Self::Unsupported => "Unsupported",
        }
    }

    pub fn shows_strip(self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Ready | Self::Restarting | Self::Failed | Self::Unsupported
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateState {
    pub phase: UpdatePhase,
    pub version: Option<String>,
    pub progress: Option<f32>,
    pub error: Option<String>,
    #[serde(skip)]
    executable: Option<PathBuf>,
}

impl UpdateState {
    pub fn new(phase: UpdatePhase) -> Self {
        Self {
            phase,
            ..Self::default()
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    pub fn with_progress(mut self, progress: Option<f32>) -> Self {
        self.progress = progress.map(|value| value.clamp(0.0, 1.0));
        self
    }

    pub fn with_error(mut self, error: impl Into<String>) -> Self {
        self.error = Some(error.into());
        self
    }

    pub fn with_executable(mut self, executable: impl Into<PathBuf>) -> Self {
        self.executable = Some(executable.into());
        self
    }

    pub fn executable(&self) -> Option<&Path> {
        self.executable.as_deref()
    }

    pub fn normalized(mut self) -> Self {
        self.progress = self.progress.map(|value| value.clamp(0.0, 1.0));
        self
    }

    pub fn progress_percent(&self) -> Option<u32> {
        self.progress
            .map(|value| (value.clamp(0.0, 1.0) * 100.0).round() as u32)
    }

    pub fn shows_strip(&self) -> bool {
        self.phase.shows_strip()
    }

    pub fn status_text(&self) -> String {
        let version = self
            .version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty());
        match self.phase {
            UpdatePhase::Idle => "Updates are checked automatically.".to_owned(),
            UpdatePhase::Checking => "Checking for updates…".to_owned(),
            UpdatePhase::UpToDate => "Up to date".to_owned(),
            UpdatePhase::Downloading => match (version, self.progress_percent()) {
                (Some(version), Some(percent)) => {
                    format!("Downloading version {version} ({percent}%)")
                }
                (Some(version), None) => format!("Downloading version {version}"),
                (None, Some(percent)) => format!("Downloading update ({percent}%)"),
                (None, None) => "Downloading update…".to_owned(),
            },
            UpdatePhase::Ready => version
                .map(|version| format!("Version {version} is ready"))
                .unwrap_or_else(|| "Update is ready".to_owned()),
            UpdatePhase::Restarting => "Restarting to update…".to_owned(),
            UpdatePhase::Failed => self
                .error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .unwrap_or("Update failed. Try again.")
                .to_owned(),
            UpdatePhase::Unsupported => self
                .error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(|error| format!("Automatic updates are not available: {error}"))
                .unwrap_or_else(|| {
                    "Automatic updates are not available on this system.".to_owned()
                }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateManifest {
    pub schema: u32,
    pub channel: String,
    pub version: Version,
    pub published_at: String,
    pub artifacts: Vec<UpdateArtifact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateArtifact {
    pub os: String,
    pub arch: String,
    #[serde(alias = "file")]
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("Update manifest JSON is invalid: {0}. Check the manifest and try again.")]
    Json(#[from] serde_json::Error),

    #[error(
        "Update manifest schema mismatch. Expected {expected}. Received {actual}. Use schema {expected}."
    )]
    SchemaMismatch { expected: u32, actual: u32 },

    #[error(
        "Update manifest channel mismatch. Expected {expected}. Received {actual}. Use channel {expected}."
    )]
    ChannelMismatch { expected: String, actual: String },

    #[error("The expected update manifest channel is empty. Set a channel and try again.")]
    EmptyExpectedChannel,

    #[error("Update manifest version is invalid: {value}: {source}. Use a semantic version.")]
    InvalidVersion {
        value: String,
        #[source]
        source: semver::Error,
    },

    #[error("The update manifest has no artifacts. Add an artifact and try again.")]
    NoArtifacts,

    #[error(
        "Update manifest artifact {index} is invalid: {reason}. Correct the artifact and try again."
    )]
    InvalidArtifact { index: usize, reason: String },
}

pub fn parse_manifest(bytes: &[u8]) -> Result<UpdateManifest, UpdateError> {
    let value = serde_json::from_slice(bytes)?;
    parse_manifest_value(value)
}

fn parse_manifest_value(value: serde_json::Value) -> Result<UpdateManifest, UpdateError> {
    if let Some(version) = value.get("version").and_then(serde_json::Value::as_str) {
        parse_version(version)?;
    }
    Ok(serde_json::from_value(value)?)
}

pub fn parse_version(value: &str) -> Result<Version, UpdateError> {
    Version::parse(value).map_err(|source| UpdateError::InvalidVersion {
        value: value.to_owned(),
        source,
    })
}

pub fn validate_manifest(
    manifest: &UpdateManifest,
    expected_channel: &str,
) -> Result<(), UpdateError> {
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(UpdateError::SchemaMismatch {
            expected: MANIFEST_SCHEMA,
            actual: manifest.schema,
        });
    }
    if expected_channel.is_empty() {
        return Err(UpdateError::EmptyExpectedChannel);
    }
    if manifest.channel != expected_channel {
        return Err(UpdateError::ChannelMismatch {
            expected: expected_channel.to_owned(),
            actual: manifest.channel.clone(),
        });
    }
    if manifest.artifacts.is_empty() {
        return Err(UpdateError::NoArtifacts);
    }
    for (index, artifact) in manifest.artifacts.iter().enumerate() {
        validate_artifact_at(index, artifact)?;
    }
    Ok(())
}

pub fn parse_and_validate_manifest(
    bytes: &[u8],
    expected_channel: &str,
) -> Result<UpdateManifest, UpdateError> {
    let manifest = parse_manifest(bytes)?;
    validate_manifest(&manifest, expected_channel)?;
    Ok(manifest)
}

fn validate_artifact_at(index: usize, artifact: &UpdateArtifact) -> Result<(), UpdateError> {
    let invalid = |reason: &str| UpdateError::InvalidArtifact {
        index,
        reason: reason.to_owned(),
    };
    if artifact.os.is_empty() || artifact.os.chars().any(char::is_control) {
        return Err(invalid(
            "os must not be empty or contain control characters",
        ));
    }
    if artifact.arch.is_empty() || artifact.arch.chars().any(char::is_control) {
        return Err(invalid(
            "arch must not be empty or contain control characters",
        ));
    }
    if artifact.name.is_empty()
        || artifact.name == "."
        || artifact.name == ".."
        || artifact.name.contains('/')
        || artifact.name.contains('\\')
        || artifact.name.chars().any(char::is_control)
    {
        return Err(invalid("name must be one safe file name"));
    }
    if artifact.url.trim().is_empty()
        || artifact
            .url
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(invalid(
            "url must not be empty or contain spaces or control characters",
        ));
    }
    if artifact.size == 0 {
        return Err(invalid("size must be greater than zero"));
    }
    if !is_valid_sha256(&artifact.sha256) {
        return Err(invalid("sha256 must be a 64-character hexadecimal string"));
    }
    Ok(())
}

pub fn is_valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn is_newer_version(current: &Version, candidate: &Version) -> bool {
    candidate > current
}

pub fn controlled_version_dir(version: &Version) -> String {
    format!("v{version}")
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateStateDto {
    pub etag: Option<String>,
    pub latest_version: Option<String>,
}

#[derive(Debug, Error)]
pub enum UpdateStateError {
    #[error(
        "The update state directory is unavailable. Check the application data directory and try again."
    )]
    StateDirUnavailable,

    #[error("Update state file I/O failed: {0}. Check the file and try again.")]
    Io(#[from] io::Error),

    #[error("Update state JSON is invalid: {0}. Check the file and try again.")]
    Json(#[from] serde_json::Error),

    #[error("Update state version is invalid: {value}. Use a valid semantic version.")]
    InvalidVersion { value: String },
}

pub fn default_update_state_path() -> Option<PathBuf> {
    paths::state_dir().map(|path| path.join(UPDATE_STATE_FILE))
}

pub fn read_update_state() -> UpdateStateDto {
    default_update_state_path().map_or_else(UpdateStateDto::default, |path| {
        read_update_state_from(&path)
    })
}

pub fn read_update_state_from(path: &Path) -> UpdateStateDto {
    match try_read_update_state_from(path) {
        Ok(state) => state,
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "Update state file is corrupt. Using default values.");
            let _ = fs::remove_file(path);
            UpdateStateDto::default()
        }
    }
}

pub fn try_read_update_state_from(path: &Path) -> Result<UpdateStateDto, UpdateStateError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(UpdateStateDto::default());
        }
        Err(error) => return Err(error.into()),
    };
    let state: UpdateStateDto = serde_json::from_slice(&bytes)?;
    if let Some(version) = state.latest_version.as_deref() {
        parse_version(version).map_err(|_| UpdateStateError::InvalidVersion {
            value: version.to_owned(),
        })?;
    }
    Ok(state)
}

pub fn write_update_state(state: &UpdateStateDto) -> Result<(), UpdateStateError> {
    let path = default_update_state_path().ok_or(UpdateStateError::StateDirUnavailable)?;
    write_update_state_to(&path, state)
}

pub fn write_update_state_to(path: &Path, state: &UpdateStateDto) -> Result<(), UpdateStateError> {
    write_json_atomic(path, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn artifact() -> UpdateArtifact {
        UpdateArtifact {
            os: UPDATE_TARGET_OS.to_owned(),
            arch: UPDATE_TARGET_ARCH.to_owned(),
            name: "k8s-app.tar.gz".to_owned(),
            url: "https://example.invalid/k8s-app.tar.gz".to_owned(),
            size: 42,
            sha256: "a".repeat(64),
        }
    }

    fn manifest(version: &str) -> UpdateManifest {
        UpdateManifest {
            schema: MANIFEST_SCHEMA,
            channel: DEFAULT_CHANNEL.to_owned(),
            version: parse_version(version).expect("version"),
            published_at: "2026-09-24T00:00:00Z".to_owned(),
            artifacts: vec![artifact()],
        }
    }

    #[test]
    fn manifest_parses_and_validates_artifacts() {
        let bytes = serde_json::to_vec(&manifest("1.2.3")).expect("serialize");
        let parsed = parse_and_validate_manifest(&bytes, DEFAULT_CHANNEL).expect("valid");
        assert_eq!(parsed.version, parse_version("1.2.3").expect("version"));
        assert_eq!(parsed.artifacts, vec![artifact()]);
    }

    #[test]
    fn manifest_validation_is_strict_for_schema_channel_and_artifacts() {
        let invalid_version = br#"{"schema":1,"channel":"stable","version":"not-semver","published_at":"","artifacts":[]}"#;
        assert!(matches!(
            parse_manifest(invalid_version),
            Err(UpdateError::InvalidVersion { .. })
        ));

        let mut value = manifest("1.2.3");
        value.schema += 1;
        assert!(matches!(
            validate_manifest(&value, DEFAULT_CHANNEL),
            Err(UpdateError::SchemaMismatch { .. })
        ));

        value = manifest("1.2.3");
        value.channel = "beta".to_owned();
        assert!(matches!(
            validate_manifest(&value, DEFAULT_CHANNEL),
            Err(UpdateError::ChannelMismatch { .. })
        ));

        value = manifest("1.2.3");
        value.artifacts[0].sha256 = "not-a-sha256".to_owned();
        assert!(matches!(
            validate_manifest(&value, DEFAULT_CHANNEL),
            Err(UpdateError::InvalidArtifact { .. })
        ));

        value = manifest("1.2.3");
        value.artifacts.clear();
        assert!(matches!(
            validate_manifest(&value, DEFAULT_CHANNEL),
            Err(UpdateError::NoArtifacts)
        ));
    }

    #[test]
    fn semver_and_sha256_helpers_use_strict_comparison_and_format() {
        let current = parse_version("1.10.0").expect("version");
        assert!(is_newer_version(
            &current,
            &parse_version("1.10.1").expect("version")
        ));
        assert!(!is_newer_version(
            &current,
            &parse_version("1.9.9").expect("version")
        ));
        assert!(!is_newer_version(
            &current,
            &parse_version("1.10.0-rc.1").expect("version")
        ));
        assert!(is_valid_sha256(&"A".repeat(64)));
        assert!(!is_valid_sha256(&"a".repeat(63)));
        assert!(!is_valid_sha256(&"g".repeat(64)));
        assert_eq!(
            controlled_version_dir(&parse_version("1.2.3-rc.1").expect("version")),
            "v1.2.3-rc.1"
        );
    }

    #[test]
    fn state_round_trip_and_corrupt_file_falls_back() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(UPDATE_STATE_FILE);
        let state = UpdateStateDto {
            etag: Some("etag-1".to_owned()),
            latest_version: Some("1.2.3".to_owned()),
        };
        write_update_state_to(&path, &state).expect("write");
        assert_eq!(read_update_state_from(&path), state);

        fs::write(&path, b"not json").expect("corrupt");
        assert_eq!(read_update_state_from(&path), UpdateStateDto::default());
        assert!(!path.exists(), "the corrupted file is removed");
    }

    #[test]
    fn state_file_uses_defaults_for_missing_fields() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join(UPDATE_STATE_FILE);
        fs::write(&path, serde_json::to_vec(&json!({})).expect("json")).expect("write");
        assert_eq!(read_update_state_from(&path), UpdateStateDto::default());
    }
}
