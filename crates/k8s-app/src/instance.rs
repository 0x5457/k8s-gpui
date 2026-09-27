//! Per-user single-instance guard and the shared cluster registry handle.
//!
//! A second launch cannot take the guard, so it exits before opening a window.

#[cfg(any(windows, test))]
use std::io;
use std::sync::{Arc, RwLock};

use gpui::{App, Global};
use k8s_core::cluster::ClusterRegistry;
#[cfg(unix)]
use k8s_core::ipc::{self, InstanceLock, IpcError};

#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

#[cfg(test)]
const WINDOWS_ERROR_ALREADY_EXISTS: i32 = 183;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallResult {
    Started,
    AlreadyRunning,
    Failed,
}

/// Shares the loaded cluster registry between the startup task and the shell.
#[derive(Clone, Default)]
pub struct RegistryHandle {
    current: Arc<RwLock<Option<Arc<ClusterRegistry>>>>,
}

impl RegistryHandle {
    pub fn new(registry: Option<Arc<ClusterRegistry>>) -> Self {
        Self {
            current: Arc::new(RwLock::new(registry)),
        }
    }

    pub fn current(&self) -> Option<Arc<ClusterRegistry>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn replace(&self, registry: Arc<ClusterRegistry>) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(registry);
    }
}

#[cfg(windows)]
struct InstanceMutex {
    handle: HANDLE,
}

#[cfg(windows)]
impl Drop for InstanceMutex {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Held in a GPUI global so the guard lives as long as the window does.
struct InstanceGuard {
    #[cfg(unix)]
    _lock: InstanceLock,
    #[cfg(windows)]
    _instance_mutex: InstanceMutex,
}

struct InstanceGuardGlobal {
    _guard: InstanceGuard,
}

impl Global for InstanceGuardGlobal {}

/// Claims the single-instance guard. Failures are logged and do not stop the GUI.
pub fn install(cx: &mut App) -> InstallResult {
    #[cfg(unix)]
    {
        install_unix(cx)
    }
    #[cfg(windows)]
    {
        install_windows(cx)
    }
}

#[cfg(unix)]
fn install_unix(cx: &mut App) -> InstallResult {
    let dir = match ipc::socket_dir_from_env() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("k8s-gpui: single-instance guard failed: {error}");
            return InstallResult::Failed;
        }
    };
    if let Err(error) = ipc::prepare_socket_dir(&dir) {
        eprintln!(
            "k8s-gpui: single-instance guard failed. Cannot create {}: {error}",
            dir.display()
        );
        return InstallResult::Failed;
    }
    let lock = match InstanceLock::acquire(&dir.join(ipc::IPC_LOCK_NAME)) {
        Ok(lock) => lock,
        Err(error) => {
            let result = classify_unix_error(&error);
            eprintln!("k8s-gpui: single-instance guard failed: {error}");
            return result;
        }
    };
    cx.set_global(InstanceGuardGlobal {
        _guard: InstanceGuard { _lock: lock },
    });
    InstallResult::Started
}

#[cfg(windows)]
fn install_windows(cx: &mut App) -> InstallResult {
    let mutex_name = match instance_mutex_name() {
        Ok(name) => name,
        Err(error) => {
            eprintln!("k8s-gpui: single-instance guard failed: {error}");
            return InstallResult::Failed;
        }
    };
    let instance_mutex = match acquire_instance_mutex(&mutex_name) {
        Ok(instance_mutex) => instance_mutex,
        Err(error) => {
            let result = classify_mutex_error(&error);
            eprintln!("k8s-gpui: single-instance guard failed: {error}");
            return result;
        }
    };
    cx.set_global(InstanceGuardGlobal {
        _guard: InstanceGuard {
            _instance_mutex: instance_mutex,
        },
    });
    InstallResult::Started
}

#[cfg(unix)]
fn classify_unix_error(error: &IpcError) -> InstallResult {
    match error {
        IpcError::AlreadyRunning => InstallResult::AlreadyRunning,
        _ => InstallResult::Failed,
    }
}

#[cfg(any(windows, test))]
fn classify_mutex_error(error: &io::Error) -> InstallResult {
    if error.raw_os_error() == Some(WINDOWS_ERROR_ALREADY_EXISTS) {
        InstallResult::AlreadyRunning
    } else {
        InstallResult::Failed
    }
}

#[cfg(windows)]
fn acquire_instance_mutex(name: &str) -> io::Result<InstanceMutex> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::PCWSTR;

    let name = std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let handle =
        unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.map_err(io::Error::other)?;
    let code = unsafe { GetLastError() };
    if code != ERROR_SUCCESS {
        let _ = unsafe { CloseHandle(handle) };
        return Err(io::Error::from_raw_os_error(code.0 as i32));
    }
    Ok(InstanceMutex { handle })
}

/// Names the mutex after the current user so sessions cannot collide.
#[cfg(windows)]
fn instance_mutex_name() -> io::Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    use windows::Win32::Foundation::HANDLE as _;
    use windows::Win32::Security::TOKEN_QUERY;
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(io::Error::other)?;
    let sid = token_user_sid(token);
    let _ = unsafe { windows::Win32::Foundation::CloseHandle(token) };
    let sid = sid?;

    let mut name = String::from(r"Local\k8s-gpui-v1-");
    for byte in sid {
        name.push(HEX[(byte >> 4) as usize] as char);
        name.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(format!("{name}-mutex"))
}

/// Copies the token user SID out of the token handle.
#[cfg(windows)]
fn token_user_sid(token: HANDLE) -> io::Result<Vec<u8>> {
    use windows::Win32::Security::{GetLengthSid, GetTokenInformation, TOKEN_USER, TokenUser};

    let mut required = 0;
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut storage = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
    let storage_capacity = storage.len() * std::mem::size_of::<usize>();
    if let Err(error) = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(storage.as_mut_ptr() as *mut std::ffi::c_void),
            required,
            &mut required,
        )
    } {
        return Err(io::Error::other(error));
    }
    let (sid_pointer, sid_length) = unsafe {
        let token_user = &*(storage.as_ptr() as *const TOKEN_USER);
        (
            token_user.User.Sid,
            GetLengthSid(token_user.User.Sid) as usize,
        )
    };
    if sid_length == 0 || sid_length > required as usize || sid_length > storage_capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "User SID buffer length is invalid",
        ));
    }
    let mut sid_storage = vec![0u8; sid_length];
    unsafe {
        std::ptr::copy_nonoverlapping(
            sid_pointer.0 as *const u8,
            sid_storage.as_mut_ptr(),
            sid_length,
        );
    }
    Ok(sid_storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_install_status_only_marks_live_lock_as_existing() {
        assert_eq!(
            classify_unix_error(&IpcError::AlreadyRunning),
            InstallResult::AlreadyRunning
        );
        assert_eq!(
            classify_unix_error(&IpcError::RuntimeDir("unavailable".to_owned())),
            InstallResult::Failed
        );
    }

    async fn test_registry(context: &str) -> Arc<ClusterRegistry> {
        let yaml = format!(
            r#"
apiVersion: v1
kind: Config
clusters:
- name: cluster
  cluster:
    server: http://127.0.0.1:6443
contexts:
- name: {context}
  context: {{ cluster: cluster, user: user }}
users:
- name: user
  user: {{}}
current-context: {context}
"#
        );
        let kubeconfig = kube::config::Kubeconfig::from_yaml(&yaml).expect("kubeconfig");
        Arc::new(ClusterRegistry::from_kubeconfig(kubeconfig).await)
    }

    fn cluster_names(registry: &ClusterRegistry) -> Vec<String> {
        registry
            .clusters()
            .iter()
            .map(|cluster| cluster.name().to_string())
            .collect()
    }

    #[tokio::test]
    async fn registry_handle_replaces_current_registry() {
        let handle = RegistryHandle::default();
        assert!(handle.current().is_none());

        handle.replace(test_registry("alpha").await);
        assert_eq!(
            cluster_names(&handle.current().expect("alpha registry")),
            ["alpha"]
        );

        handle.replace(test_registry("beta").await);
        assert_eq!(
            cluster_names(&handle.current().expect("beta registry")),
            ["beta"]
        );
    }

    #[tokio::test]
    async fn reload_commit_replaces_handle_and_failure_preserves_it() {
        let handle = RegistryHandle::default();
        handle.replace(test_registry("before").await);
        assert_eq!(
            cluster_names(&handle.current().expect("before registry")),
            ["before"]
        );

        handle.replace(test_registry("after").await);
        assert_eq!(
            cluster_names(&handle.current().expect("after registry")),
            ["after"]
        );

        let failed: Result<Arc<ClusterRegistry>, String> = Err("reload failed".to_owned());
        if let Ok(registry) = failed {
            handle.replace(registry);
        }
        assert_eq!(
            cluster_names(&handle.current().expect("after registry")),
            ["after"]
        );
    }

    #[tokio::test]
    async fn registry_handle_snapshot_survives_replacement() {
        let handle = RegistryHandle::default();
        handle.replace(test_registry("alpha").await);
        let in_flight = handle.current().expect("alpha registry");

        handle.replace(test_registry("beta").await);

        assert_eq!(cluster_names(&in_flight), ["alpha"]);
        assert_eq!(
            cluster_names(&handle.current().expect("beta registry")),
            ["beta"]
        );
    }

    #[test]
    fn windows_install_status_only_marks_existing_mutex_as_existing() {
        assert_eq!(
            classify_mutex_error(&io::Error::from_raw_os_error(WINDOWS_ERROR_ALREADY_EXISTS)),
            InstallResult::AlreadyRunning
        );
        for error in [
            io::Error::from_raw_os_error(5),
            io::Error::from_raw_os_error(1),
            io::Error::other("mutex unavailable"),
        ] {
            assert_eq!(classify_mutex_error(&error), InstallResult::Failed);
        }
    }
}
