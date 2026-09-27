//! Defines the per-user single-instance lock and its runtime directory.

use std::io;
use std::path::{Path, PathBuf};

/// Lock file name inside the runtime directory.
pub(crate) const IPC_DIR_NAME: &str = "k8s-gpui";
pub const IPC_LOCK_NAME: &str = "ipc.lock";

/// Directory and lock file modes restrict access to the current user.
pub(crate) const SOCKET_DIR_MODE: u32 = 0o700;
pub(crate) const LOCK_MODE: u32 = 0o600;

/// Environment variable that overrides the runtime directory.
pub(crate) const IPC_DIR_ENV: &str = "K8S_GPUI_IPC_DIR";
pub(crate) const XDG_RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("Runtime directory is unavailable: {0}. Set XDG_RUNTIME_DIR and try again.")]
    RuntimeDir(String),

    #[error("Lock file I/O failed: {0}. Check the runtime directory permissions, then try again.")]
    Io(#[from] io::Error),

    #[error(
        "Another k8s-gpui instance is already running. Close the other instance and try again."
    )]
    AlreadyRunning,
}

/// Build the runtime directory from a parent runtime directory.
pub fn socket_dir(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(IPC_DIR_NAME)
}

/// Resolve the runtime directory from the environment.
pub fn socket_dir_from_env() -> Result<PathBuf, IpcError> {
    if let Some(dir) = std::env::var_os(IPC_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    #[cfg(target_os = "linux")]
    {
        let runtime_dir = std::env::var_os(XDG_RUNTIME_DIR_ENV)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| IpcError::RuntimeDir(format!("{XDG_RUNTIME_DIR_ENV} is not set")))?;
        Ok(socket_dir(Path::new(&runtime_dir)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(socket_dir(&std::env::temp_dir()))
    }
}

#[cfg(unix)]
pub fn prepare_socket_dir(dir: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(SOCKET_DIR_MODE))?;
    Ok(())
}

#[cfg(not(unix))]
pub fn prepare_socket_dir(_dir: &Path) -> Result<(), IpcError> {
    Ok(())
}

/// Exclusive advisory lock held for the lifetime of the running GUI.
///
/// A second launch cannot acquire the lock, which is how the app detects that
/// an instance is already running.
#[cfg(unix)]
#[derive(Debug)]
pub struct InstanceLock {
    _file: std::fs::File,
}

#[cfg(unix)]
impl InstanceLock {
    #[allow(unsafe_code)]
    pub fn acquire(path: &Path) -> Result<Self, IpcError> {
        use std::os::fd::AsRawFd as _;
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(LOCK_MODE)
            .open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(LOCK_MODE))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(Self { _file: file });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK)
            || error.raw_os_error() == Some(libc::EAGAIN)
        {
            return Err(IpcError::AlreadyRunning);
        }
        Err(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("k8s-gpui-ipc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test directory");
        dir
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_is_private() {
        let dir = temp_dir("modes").join(IPC_DIR_NAME);
        prepare_socket_dir(&dir).expect("create directory");
        assert_eq!(
            mode_of(&dir),
            SOCKET_DIR_MODE,
            "directory must use mode 0700"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_is_exclusive_and_released_without_deleting_the_file() {
        let path = temp_dir("lock-live").join(IPC_LOCK_NAME);
        let lock = InstanceLock::acquire(&path).expect("first acquire");
        assert_eq!(mode_of(&path), LOCK_MODE);

        let error = InstanceLock::acquire(&path).expect_err("the second instance is rejected");
        assert!(matches!(error, IpcError::AlreadyRunning), "{error:?}");

        drop(lock);
        assert!(path.exists());
        let lock = InstanceLock::acquire(&path).expect("acquire again after release");
        drop(lock);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn existing_lock_file_contents_are_not_parsed_or_removed() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_dir("lock-existing").join(IPC_LOCK_NAME);
        std::fs::write(&path, "not-a-pid").expect("write placeholder content");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
            .expect("relax placeholder permissions");
        let lock = InstanceLock::acquire(&path).expect("lock the existing file");
        assert_eq!(mode_of(&path), LOCK_MODE);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "not-a-pid");
        drop(lock);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "not-a-pid");
    }
}
