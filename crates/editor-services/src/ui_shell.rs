use crate::{ErrorKind, FaultInjector, FaultPoint, ServiceError};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, TryLockError},
};

pub const MAX_RECENT_FILES: usize = 30;
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_FLUSH_WAITERS: usize = 32;
const STATE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellTheme {
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiShellState {
    pub version: u32,
    pub sidebar_visible: bool,
    pub soft_wrap: bool,
    pub show_whitespaces: bool,
    pub tab_size: u8,
    pub hard_tabs: bool,
    pub theme_mode: Option<ShellTheme>,
    pub recent_files: Vec<PathBuf>,
}
impl Default for UiShellState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            sidebar_visible: true,
            // EditorAdapter uses gpui-base 0.9.0 EditorState defaults for these.
            soft_wrap: true,
            show_whitespaces: false,
            tab_size: 2,
            hard_tabs: false,
            theme_mode: None,
            recent_files: Vec::new(),
        }
    }
}
impl UiShellState {
    /// Paths are compared lexically. History never stats or canonicalizes its targets.
    pub fn record_recent(&mut self, path: PathBuf) {
        if !valid_recent_path(&path) {
            return;
        }
        self.recent_files.retain(|previous| previous != &path);
        self.recent_files.truncate(MAX_RECENT_FILES - 1);
        self.recent_files.insert(0, path);
    }

    pub fn clear_recent(&mut self) {
        self.recent_files.clear();
    }
}

#[derive(Clone, Debug)]
pub struct UiShellLoad {
    pub state: UiShellState,
    pub warning: Option<String>,
}

/// One bounded latest-state slot and one filesystem writer, shared by all clones.
/// Queueing is admission, not durability: observe a flush/shutdown reply off the UI thread.
#[derive(Clone)]
pub struct UiShellStore {
    inner: Arc<StoreInner>,
}
struct StoreInner {
    mailbox: Arc<Mutex<Mailbox>>,
    wake: Sender<()>,
}
#[derive(Default)]
struct Mailbox {
    pending: Option<Vec<u8>>,
    replies: Vec<Sender<Result<(), ServiceError>>>,
    shutdown_reply: Option<Sender<Result<(), ServiceError>>>,
    closed: bool,
}
impl UiShellStore {
    /// Blocking startup only. Loading never creates directories, rewrites settings, or
    /// touches recent-file targets. Damaged input is backed up on the first queued change.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, UiShellLoad), ServiceError> {
        Self::open_with_faults(path.as_ref(), FaultInjector::default())
    }

    fn open_with_faults(
        path: &Path,
        faults: FaultInjector,
    ) -> Result<(Self, UiShellLoad), ServiceError> {
        let path = std::path::absolute(path)?;
        if path.file_name().is_none() {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "UI preferences path must name a file",
            ));
        }
        let load = load_state(&path)?;
        let preserve_original = load.warning.is_some();
        let (wake, receiver) = bounded(1);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let worker_mailbox = mailbox.clone();
        std::thread::Builder::new()
            .name("ui-shell-writer".into())
            .spawn(move || {
                run_writer(path, preserve_original, worker_mailbox, receiver, faults);
            })?;
        Ok((
            Self {
                inner: Arc::new(StoreInner { mailbox, wake }),
            },
            load,
        ))
    }

    /// Bounded CPU work only; no filesystem work or waiting for a writer. A newer
    /// accepted snapshot replaces the pending one. Retain/retry on `Busy`.
    pub fn queue(&self, state: UiShellState) -> Result<(), ServiceError> {
        let bytes = encode_state(&state)?;
        let mut mailbox = self.mailbox()?;
        ensure_open(&mailbox)?;
        mailbox.pending = Some(bytes);
        self.wake_writer()
    }

    /// Acknowledges publication of all snapshots accepted before this barrier (or a
    /// newer coalesced snapshot). Writer failures remain visible until a later write succeeds.
    pub fn flush(&self) -> Result<Receiver<Result<(), ServiceError>>, ServiceError> {
        self.barrier(false)
    }

    /// Stops admission immediately and drains the last accepted state before replying.
    /// No writes occur after its reply. Await with a bounded timeout off the UI thread.
    pub fn shutdown(&self) -> Result<Receiver<Result<(), ServiceError>>, ServiceError> {
        self.barrier(true)
    }

    fn barrier(&self, shutdown: bool) -> Result<Receiver<Result<(), ServiceError>>, ServiceError> {
        let mut mailbox = self.mailbox()?;
        ensure_open(&mailbox)?;
        if !shutdown && mailbox.replies.len() == MAX_FLUSH_WAITERS {
            return Err(shell_busy());
        }
        let (reply, receiver) = bounded(1);
        if shutdown {
            mailbox.closed = true;
            mailbox.shutdown_reply = Some(reply);
        } else {
            mailbox.replies.push(reply);
        }
        self.wake_writer()?;
        Ok(receiver)
    }

    fn mailbox(&self) -> Result<MutexGuard<'_, Mailbox>, ServiceError> {
        self.inner.mailbox.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => shell_busy(),
            TryLockError::Poisoned(_) => ServiceError::new(
                ErrorKind::Storage,
                "UI preferences writer state is unavailable",
            ),
        })
    }

    fn wake_writer(&self) -> Result<(), ServiceError> {
        match self.inner.wake.try_send(()) {
            Ok(()) | Err(TrySendError::Full(())) => Ok(()),
            Err(TrySendError::Disconnected(())) => Err(ServiceError::new(
                ErrorKind::Storage,
                "UI preferences writer stopped before accepting the request",
            )),
        }
    }
}

fn shell_busy() -> ServiceError {
    ServiceError::new(
        ErrorKind::Busy,
        "UI preferences writer is busy; retain the request and retry",
    )
}
fn ensure_open(mailbox: &Mailbox) -> Result<(), ServiceError> {
    if mailbox.closed {
        Err(ServiceError::new(
            ErrorKind::Cancelled,
            "UI preferences writer has shut down",
        ))
    } else {
        Ok(())
    }
}
fn valid_recent_path(path: &Path) -> bool {
    path.to_str().is_some_and(|text| {
        !text.is_empty() && !text.contains('\0') && text.len() <= MAX_STATE_BYTES
    })
}

fn load_state(path: &Path) -> Result<UiShellLoad, ServiceError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UiShellLoad {
                state: UiShellState::default(),
                warning: None,
            });
        }
        Err(error) => return Err(shell_io("opening settings", error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| shell_io("reading settings metadata", error))?;
    if !metadata.is_file() {
        return Err(ServiceError::new(
            ErrorKind::InvalidInput,
            "UI preferences path is not a regular file",
        ));
    }
    if metadata.len() > MAX_STATE_BYTES as u64 {
        return Ok(damaged_load("settings exceed the 256 KiB limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&file)
        .take(MAX_STATE_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| shell_io("reading settings", error))?;
    if file.metadata()?.len() > MAX_STATE_BYTES as u64 {
        return Ok(damaged_load("settings exceed the 256 KiB limit"));
    }
    let mut state: UiShellState = match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(error) => return Ok(damaged_load(&format!("invalid settings: {error}"))),
    };
    if state.version != STATE_VERSION {
        return Ok(damaged_load(&format!(
            "unsupported settings version {}",
            state.version
        )));
    }
    let mut corrections = Vec::new();
    if !matches!(state.tab_size, 2 | 4 | 8) {
        state.tab_size = UiShellState::default().tab_size;
        corrections.push("invalid tab size reset to 2");
    }
    let recent = std::mem::take(&mut state.recent_files);
    let mut corrected_recent = recent.len() > MAX_RECENT_FILES;
    for path in recent {
        if !valid_recent_path(&path) || state.recent_files.contains(&path) {
            corrected_recent = true;
        } else if state.recent_files.len() < MAX_RECENT_FILES {
            state.recent_files.push(path);
        }
    }
    if corrected_recent {
        corrections.push("invalid, duplicate, or excess recent paths removed");
    }
    let warning = (!corrections.is_empty()).then(|| {
        format!(
            "UI preferences: {}. Original retained and will be backed up before the next change.",
            corrections.join("; ")
        )
    });
    Ok(UiShellLoad { state, warning })
}
fn damaged_load(reason: &str) -> UiShellLoad {
    UiShellLoad {
        state: UiShellState::default(),
        warning: Some(format!(
            "UI preferences: {reason}. Using defaults; original retained and will be backed up before the next change."
        )),
    }
}

struct LimitedBytes(Vec<u8>);
impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_STATE_BYTES - self.0.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "UI preferences exceed the 256 KiB serialized limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode_state(state: &UiShellState) -> Result<Vec<u8>, ServiceError> {
    if state.version != STATE_VERSION
        || !matches!(state.tab_size, 2 | 4 | 8)
        || state.recent_files.len() > MAX_RECENT_FILES
        || state.recent_files.iter().enumerate().any(|(index, path)| {
            !valid_recent_path(path) || state.recent_files[..index].contains(path)
        })
    {
        return Err(ServiceError::new(
            ErrorKind::InvalidInput,
            "UI preferences contain an unsupported version, tab size, or recent-file list",
        ));
    }
    let mut bytes = LimitedBytes(Vec::new());
    serde_json::to_writer(&mut bytes, state).map_err(|error| {
        ServiceError::new(ErrorKind::InvalidInput, format!("UI preferences: {error}"))
    })?;
    Ok(bytes.0)
}

fn run_writer(
    path: PathBuf,
    mut preserve_original: bool,
    mailbox: Arc<Mutex<Mailbox>>,
    wake: Receiver<()>,
    faults: FaultInjector,
) {
    let mut result = Ok(());
    let mut replies = Vec::new();
    loop {
        // Dropping the last handle also drains accepted state and releases the worker.
        let disconnected = wake.recv().is_err();
        let (pending, shutdown_reply) = {
            let mut mailbox = mailbox.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::swap(&mut replies, &mut mailbox.replies);
            (mailbox.pending.take(), mailbox.shutdown_reply.take())
        };
        if let Some(bytes) = pending {
            result = publish_snapshot(&path, &bytes, &mut preserve_original, &faults);
        }
        for reply in replies.drain(..) {
            let _ = reply.send(result.clone());
        }
        if let Some(reply) = shutdown_reply {
            let _ = reply.send(result);
            return;
        }
        if disconnected {
            return;
        }
    }
}

struct PendingFile {
    path: PathBuf,
    file: File,
    published: bool,
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn publish_snapshot(
    path: &Path,
    bytes: &[u8],
    preserve_original: &mut bool,
    faults: &FaultInjector,
) -> Result<(), ServiceError> {
    let parent = path.parent().ok_or_else(|| {
        ServiceError::new(ErrorKind::InvalidInput, "UI preferences path has no parent")
    })?;
    std::fs::create_dir_all(parent)
        .map_err(|error| shell_io("creating settings directory", error))?;
    #[cfg(unix)]
    let directory =
        File::open(parent).map_err(|error| shell_io("opening settings directory", error))?;
    let temp_path = parent.join(format!(".editor-ui-shell-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&temp_path)
        .map_err(|error| shell_io("creating settings temporary file", error))?;
    let mut temp = PendingFile {
        path: temp_path,
        file,
        published: false,
    };
    temp.file
        .write_all(bytes)
        .map_err(|error| shell_io("writing settings", error))?;
    temp.file
        .flush()
        .map_err(|error| shell_io("flushing settings", error))?;
    temp.file
        .sync_all()
        .map_err(|error| shell_io("syncing settings", error))?;
    faults.check(FaultPoint::UiShellBeforePublish)?;
    if *preserve_original {
        // An exclusive hard link retains even oversized corrupt input without copying it
        // into memory or first removing the live path. Publication only renames the temp.
        let backup = parent.join(format!(
            ".editor-ui-shell-corrupt-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::hard_link(path, &backup).map_err(|error| {
            shell_io(
                "preserving damaged settings; original was not replaced",
                error,
            )
        })?;
        #[cfg(unix)]
        directory
            .sync_all()
            .map_err(|error| shell_io("syncing damaged-settings backup", error))?;
        *preserve_original = false;
    }
    std::fs::rename(&temp.path, path)
        .map_err(|error| shell_io("publishing settings; previous file was not replaced", error))?;
    temp.published = true;
    #[cfg(unix)]
    directory.sync_all().map_err(|error| {
        ServiceError::new(
            ErrorKind::PublishedNotDurable,
            format!(
                "UI preferences were published but directory durability is unconfirmed: {error}"
            ),
        )
    })?;
    Ok(())
}
fn shell_io(operation: &str, error: std::io::Error) -> ServiceError {
    ServiceError::new(
        ErrorKind::Io,
        format!("UI preferences: {operation}: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, Instant},
    };
    use tempfile::TempDir;

    fn admit<T>(mut operation: impl FnMut() -> Result<T, ServiceError>) -> T {
        let start = Instant::now();
        loop {
            match operation() {
                Ok(value) => return value,
                Err(error)
                    if error.kind == ErrorKind::Busy
                        && start.elapsed() < Duration::from_secs(5) =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("shell admission: {error}"),
            }
        }
    }
    fn reply(receiver: Receiver<Result<(), ServiceError>>) -> Result<(), ServiceError> {
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("shell acknowledgement")
    }
    fn read_state(path: &Path) -> UiShellState {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }
    fn backups(dir: &Path) -> Vec<PathBuf> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".editor-ui-shell-corrupt-")
            })
            .collect()
    }

    #[test]
    fn defaults_and_mru_order_dedup_and_cap_need_no_target_access() {
        let mut state: UiShellState = serde_json::from_str("{}").unwrap();
        assert_eq!(state, UiShellState::default());
        assert!(state.sidebar_visible);
        assert!(state.soft_wrap);
        assert!(!state.show_whitespaces);
        assert_eq!(state.tab_size, 2);
        assert!(!state.hard_tabs);
        let dir = TempDir::new().unwrap();
        for index in 0..40 {
            state.record_recent(dir.path().join(format!("不存在-{index}.txt")));
        }
        let expected: Vec<_> = (10..40)
            .rev()
            .map(|index| dir.path().join(format!("不存在-{index}.txt")))
            .collect();
        assert_eq!(state.recent_files, expected);
        let promoted = expected[14].clone();
        state.record_recent(promoted.clone());
        state.record_recent(promoted.clone());
        assert_eq!(state.recent_files[0], promoted);
        assert_eq!(state.recent_files.len(), MAX_RECENT_FILES);
        assert_eq!(state.recent_files[1], expected[0]);
        assert_eq!(state.recent_files[29], expected[29]);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        state.clear_recent();
        assert!(state.recent_files.is_empty());
    }

    #[test]
    fn opening_missing_settings_and_empty_barriers_do_not_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("not-created").join("ui_shell.json");
        let (store, load) = UiShellStore::open(&path).unwrap();
        assert_eq!(load.state, UiShellState::default());
        assert!(load.warning.is_none());
        reply(admit(|| store.flush())).unwrap();
        reply(admit(|| store.shutdown())).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert_eq!(
            store.queue(load.state).unwrap_err().kind,
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn loading_invalid_preferences_and_history_warns_without_rewriting() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ui_shell.json");
        let mut recent = vec![String::new(), "bad\0path".into(), "/missing/0".into()];
        recent.extend((0..40).map(|index| format!("/missing/{index}")));
        let bytes = serde_json::to_vec(&serde_json::json!({
            "tab_size": 3,
            "sidebar_visible": false,
            "theme_mode": "Dark",
            "recent_files": recent,
        }))
        .unwrap();
        fs::write(&path, &bytes).unwrap();
        let (store, load) = UiShellStore::open(&path).unwrap();
        assert!(load.warning.is_some());
        assert_eq!(load.state.tab_size, 2);
        assert!(!load.state.sidebar_visible);
        assert_eq!(load.state.theme_mode, Some(ShellTheme::Dark));
        assert_eq!(load.state.recent_files.len(), MAX_RECENT_FILES);
        assert_eq!(load.state.recent_files[0], PathBuf::from("/missing/0"));
        assert_eq!(load.state.recent_files[29], PathBuf::from("/missing/29"));
        reply(admit(|| store.flush())).unwrap();
        reply(admit(|| store.shutdown())).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(backups(dir.path()).is_empty());
    }

    #[test]
    fn corrupt_and_oversized_input_is_retained_then_backed_up_once_per_store() {
        for bytes in [
            b"{broken".to_vec(),
            br#"{"version":99,"soft_wrap":false}"#.to_vec(),
            br#"{"soft_wrap":"not a boolean"}"#.to_vec(),
            vec![b'x'; MAX_STATE_BYTES + 1],
        ] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("ui_shell.json");
            let previous_backup = dir.path().join(".editor-ui-shell-corrupt-existing.json");
            fs::write(&previous_backup, b"older damaged settings").unwrap();
            fs::write(&path, &bytes).unwrap();
            let (store, load) = UiShellStore::open(&path).unwrap();
            assert!(load.warning.is_some());
            assert_eq!(load.state, UiShellState::default());
            reply(admit(|| store.shutdown())).unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(backups(dir.path()).len(), 1);

            let (store, mut load) = UiShellStore::open(&path).unwrap();
            load.state.show_whitespaces = true;
            admit(|| store.queue(load.state.clone()));
            reply(admit(|| store.flush())).unwrap();
            assert_eq!(read_state(&path), load.state);
            let retained: Vec<_> = backups(dir.path())
                .into_iter()
                .filter(|backup| backup != &previous_backup)
                .collect();
            assert_eq!(retained.len(), 1);
            assert_eq!(fs::read(&retained[0]).unwrap(), bytes);
            assert_eq!(
                fs::read(&previous_backup).unwrap(),
                b"older damaged settings"
            );
            load.state.soft_wrap = false;
            admit(|| store.queue(load.state.clone()));
            reply(admit(|| store.shutdown())).unwrap();
            assert_eq!(read_state(&path), load.state);
            assert_eq!(backups(dir.path()).len(), 2);
            assert_eq!(fs::read(&retained[0]).unwrap(), bytes);
        }
    }

    #[test]
    fn oversized_or_invalid_queued_snapshot_never_replaces_valid_state() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ui_shell.json");
        let (store, _) = UiShellStore::open(&path).unwrap();
        let good = UiShellState {
            theme_mode: Some(ShellTheme::Light),
            ..UiShellState::default()
        };
        admit(|| store.queue(good.clone()));
        // Individual paths fit; JSON escaping alone pushes the serialized state over its cap.
        let escaped = UiShellState {
            recent_files: (0..MAX_RECENT_FILES)
                .map(|index| PathBuf::from(format!("{index}{}", "\"".repeat(5_000))))
                .collect(),
            ..UiShellState::default()
        };
        assert_eq!(
            store.queue(escaped).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        for invalid in [
            UiShellState {
                version: 2,
                ..UiShellState::default()
            },
            UiShellState {
                tab_size: 0,
                ..UiShellState::default()
            },
            UiShellState {
                recent_files: vec![PathBuf::new()],
                ..UiShellState::default()
            },
            UiShellState {
                recent_files: vec!["bad\0path".into()],
                ..UiShellState::default()
            },
            UiShellState {
                recent_files: vec!["same".into(), "same".into()],
                ..UiShellState::default()
            },
            UiShellState {
                recent_files: (0..=MAX_RECENT_FILES)
                    .map(|index| PathBuf::from(index.to_string()))
                    .collect(),
                ..UiShellState::default()
            },
        ] {
            assert_eq!(
                store.queue(invalid).unwrap_err().kind,
                ErrorKind::InvalidInput
            );
        }
        reply(admit(|| store.shutdown())).unwrap();
        assert_eq!(read_state(&path), good);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn coalescing_is_latest_wins_and_shutdown_acknowledges_final_publication() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ui_shell.json");
        // Keep the real worker unscheduled until the slot has been superseded many times.
        let (wake, receiver) = bounded(1);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let store = UiShellStore {
            inner: Arc::new(StoreInner {
                mailbox: mailbox.clone(),
                wake,
            }),
        };
        let clone = store.clone();
        let barrier = store.flush().unwrap();
        let mut latest = UiShellState::default();
        for index in 0..1_000 {
            latest.soft_wrap = index % 2 == 0;
            latest.record_recent(dir.path().join(index.to_string()));
            clone.queue(latest.clone()).unwrap();
        }
        assert_eq!(receiver.len(), 1);
        assert_eq!(
            mailbox.lock().unwrap().pending.as_ref().unwrap(),
            &encode_state(&latest).unwrap()
        );
        let shutdown = store.shutdown().unwrap();
        assert_eq!(
            clone.queue(latest.clone()).unwrap_err().kind,
            ErrorKind::Cancelled
        );
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            run_writer(
                worker_path,
                false,
                mailbox,
                receiver,
                FaultInjector::default(),
            );
        });
        reply(barrier).unwrap();
        reply(shutdown).unwrap();
        worker.join().unwrap();
        assert_eq!(read_state(&path), latest);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn flush_admission_is_bounded_and_shutdown_has_its_own_slot() {
        let (wake, receiver) = bounded(1);
        let mailbox = Arc::new(Mutex::new(Mailbox::default()));
        let store = UiShellStore {
            inner: Arc::new(StoreInner {
                mailbox: mailbox.clone(),
                wake,
            }),
        };
        let replies: Vec<_> = (0..MAX_FLUSH_WAITERS)
            .map(|_| store.flush().unwrap())
            .collect();
        assert_eq!(store.flush().unwrap_err().kind, ErrorKind::Busy);
        let shutdown = store.shutdown().unwrap();
        let dir = TempDir::new().unwrap();
        run_writer(
            dir.path().join("unused"),
            false,
            mailbox,
            receiver,
            FaultInjector::default(),
        );
        for receiver in replies {
            reply(receiver).unwrap();
        }
        reply(shutdown).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn publication_failure_preserves_old_file_cleans_temp_and_remains_visible() {
        for retry in [false, true] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("ui_shell.json");
            let original = serde_json::to_vec(&UiShellState::default()).unwrap();
            fs::write(&path, &original).unwrap();
            let faults = FaultInjector::default();
            let (store, mut load) = UiShellStore::open_with_faults(&path, faults.clone()).unwrap();
            faults.arm(FaultPoint::UiShellBeforePublish);
            load.state.sidebar_visible = false;
            admit(|| store.queue(load.state.clone()));
            assert_eq!(
                reply(admit(|| store.flush())).unwrap_err().kind,
                ErrorKind::Injected
            );
            assert_eq!(fs::read(&path).unwrap(), original);
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
            assert_eq!(
                reply(admit(|| store.flush())).unwrap_err().kind,
                ErrorKind::Injected
            );
            if retry {
                load.state.tab_size = 8;
                admit(|| store.queue(load.state.clone()));
                reply(admit(|| store.shutdown())).unwrap();
                assert_eq!(read_state(&path), load.state);
            } else {
                assert_eq!(
                    reply(admit(|| store.shutdown())).unwrap_err().kind,
                    ErrorKind::Injected
                );
                assert_eq!(fs::read(&path).unwrap(), original);
            }
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }
}
