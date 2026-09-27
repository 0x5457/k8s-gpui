use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use tempfile::Builder;

#[cfg(windows)]
static REPLACE_COMMIT_LOCK: Mutex<()> = Mutex::new(());

type PathTask = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct PathTaskQueueState {
    queues: HashMap<PathBuf, VecDeque<PathTask>>,
    running: HashSet<PathBuf>,
}

pub struct PathTaskQueue {
    state: Mutex<PathTaskQueueState>,
}

impl PathTaskQueue {
    pub fn enqueue(&self, path: PathBuf, task: impl FnOnce() + Send + 'static) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let start = state.running.insert(path.clone());
        state
            .queues
            .entry(path)
            .or_default()
            .push_back(Box::new(task));
        start
    }

    pub fn pop(&self, path: &Path) -> Option<PathTask> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.queues.get_mut(path).and_then(VecDeque::pop_front)
    }

    pub fn finish(&self, path: &Path) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.queues.get(path).is_some_and(VecDeque::is_empty) {
            state.queues.remove(path);
            state.running.remove(path);
        }
    }
}

static PATH_TASK_QUEUE: LazyLock<PathTaskQueue> = LazyLock::new(|| PathTaskQueue {
    state: Mutex::new(PathTaskQueueState::default()),
});

pub fn path_task_queue() -> &'static PathTaskQueue {
    &PATH_TASK_QUEUE
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, sync_parent_directory)
}

/// Reads a JSON file the app owns, treating an absent one as an empty result.
///
/// The preference and history files are written once and then read for the rest
/// of the session, so "not there yet" is an ordinary state rather than a failure,
/// and it is `None` here rather than an error the caller has to remember to
/// absorb. A file that exists but does not parse is still an error: that one is
/// somebody's mistake, not a first run.
pub(crate) fn read_json_if_present<T, E>(path: &Path) -> Result<Option<T>, E>
where
    T: serde::de::DeserializeOwned,
    E: From<io::Error> + From<serde_json::Error>,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Writes a JSON file the app owns, creating its directory if it needs one.
///
/// The twin of [`read_json_if_present`], and the reason the preferences, the hotbar
/// banks, the applied-spec history and the update state all stopped carrying the
/// same six lines: serialise pretty, make sure the parent exists and is private,
/// write atomically. A file this writes is read back by the next run, so a partial
/// one is worse than no file at all - hence the atomic write rather than a direct
/// `fs::write`, and hence the private directory: these hold kubeconfig-derived
/// paths and cluster UIDs.
pub(crate) fn write_json_atomic<T, E>(path: &Path, value: &T) -> Result<(), E>
where
    T: serde::Serialize,
    E: From<io::Error> + From<serde_json::Error>,
{
    let json = serde_json::to_vec_pretty(value)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_dir_all(parent)?;
    }
    write_atomic(path, &json)?;
    Ok(())
}

fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut builder = Builder::new();
    builder.prefix(".k8s-gpui-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let temp_dir = builder.tempdir_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp_dir.path(), fs::Permissions::from_mode(0o700))?;
    }
    let temp_path = temp_dir.path().join("payload");
    let mut file = create_private_file(&temp_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    replace_file(path, &temp_path)?;
    drop(temp_dir);
    sync_parent(parent).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "Atomic replacement completed, but syncing parent directory {} failed: {error}",
                parent.display()
            ),
        )
    })
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let _ = path;
    Ok(())
}

pub fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(not(windows))]
fn replace_file(path: &Path, replacement: &Path) -> io::Result<()> {
    fs::rename(replacement, path)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_file(path: &Path, replacement: &Path) -> io::Result<()> {
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, REPLACE_FILE_FLAGS,
        ReplaceFileW,
    };
    use windows::core::HSTRING;

    let _commit_guard = REPLACE_COMMIT_LOCK.lock().map_err(|_| {
        io::Error::other(
            "The atomic file replacement lock is poisoned. Restart the process and try again.",
        )
    })?;
    let replaced = HSTRING::from(path.as_os_str());
    let replacement = HSTRING::from(replacement.as_os_str());
    let result = if path.try_exists()? {
        unsafe {
            ReplaceFileW(
                &replaced,
                &replacement,
                None,
                REPLACE_FILE_FLAGS::default(),
                None,
                None,
            )
        }
    } else {
        unsafe {
            MoveFileExW(
                &replacement,
                &replaced,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
    };
    result.map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::*;

    #[test]
    fn same_path_tasks_stay_serial() {
        let queue = Arc::new(PathTaskQueue {
            state: Mutex::new(PathTaskQueueState::default()),
        });
        let path = PathBuf::from("settings.json");
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (done_sender, done_receiver) = mpsc::channel();

        assert!(queue.enqueue(
            path.clone(),
            Box::new(move || {
                let _ = entered_sender.send(());
                let _ = release_receiver.recv();
            }),
        ));
        let worker_queue = queue.clone();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            while let Some(task) = worker_queue.pop(&worker_path) {
                task();
                worker_queue.finish(&worker_path);
            }
        });
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first task must start");

        assert!(!queue.enqueue(
            path.clone(),
            Box::new(move || {
                let _ = done_sender.send(());
            }),
        ));

        release_sender.send(()).expect("release first task");
        done_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("second task must run after the first task");
        worker.join().expect("path task worker");
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        create_private_dir_all(dir.path()).expect("private dir");
        let path = dir.path().join("state");

        write_atomic(&path, b"first").expect("first write");
        write_atomic(&path, b"second").expect("second write");

        assert_eq!(fs::read(&path).expect("read"), b"second");
        assert_eq!(fs::read_dir(dir.path()).expect("entries").count(), 1);
    }

    #[test]
    fn parent_sync_failure_is_returned_after_atomic_replacement() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("state");
        write_atomic(&path, b"old").expect("initial write");

        let error = write_atomic_with(&path, b"new", |parent| {
            assert_eq!(parent, dir.path());
            Err(io::Error::other("injected directory sync failure"))
        })
        .expect_err("directory sync failure must be returned");

        assert!(error.to_string().contains("Atomic replacement completed"));
        assert!(
            error
                .to_string()
                .contains("injected directory sync failure")
        );
        assert_eq!(fs::read(&path).expect("read"), b"new");
        assert_eq!(fs::read_dir(dir.path()).expect("entries").count(), 1);
    }
}
