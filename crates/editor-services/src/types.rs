use ropey::Rope;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentId(pub u128);
impl DocumentId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().as_u128())
    }
    pub(crate) fn key(self) -> String {
        format!("{:032x}", self.0)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Revision(pub u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LifecycleGen(pub u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceGeneration(pub u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryId(pub u64);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncodingSpec {
    #[default]
    Utf8,
    Utf8Bom,
    UnsupportedReadOnly,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
    MixedReadOnly,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReadOnlyReason {
    UnsupportedEncoding,
    MixedLineEndings,
    BinaryContent,
    LongLine,
    LargeFile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentIdentity {
    pub byte_len: u64,
    pub hash128: u128,
}
impl ContentIdentity {
    /// Worker-only operation: O(bytes), never call in the keystroke handler.
    pub fn of_rope(rope: &Rope) -> Self {
        let mut hasher = blake3::Hasher::new();
        for chunk in rope.chunks() {
            hasher.update(chunk.as_bytes());
        }
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
        Self {
            byte_len: rope.len() as u64,
            hash128: u128::from_le_bytes(bytes),
        }
    }
    pub fn matches_rope(self, rope: &Rope) -> bool {
        self.byte_len == rope.len() as u64 && self == Self::of_rope(rope)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub len: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}
impl FileFingerprint {
    pub fn of_path(path: &Path) -> Result<Self, ServiceError> {
        Self::of_metadata(&std::fs::metadata(path)?)
    }
    pub fn of_metadata(meta: &std::fs::Metadata) -> Result<Self, ServiceError> {
        if !meta.is_file() {
            return Err(ServiceError::new(
                ErrorKind::Unsupported,
                "not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                len: meta.len(),
                mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
                ctime_ns: meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128,
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let stamp = meta
                .modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| ServiceError::new(ErrorKind::Io, e.to_string()))?;
            Ok(Self {
                len: meta.len(),
                mtime_ns: stamp.as_nanos() as i128,
                ctime_ns: 0,
                dev: 0,
                ino: 0,
            })
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DocMeta {
    pub title: String,
    pub path: Option<PathBuf>,
    pub encoding: EncodingSpec,
    pub line_ending: LineEnding,
    pub origin_fingerprint: Option<FileFingerprint>,
    pub saved_identity: Option<ContentIdentity>,
    /// Captured output format of the last acknowledged save, not current preferences.
    /// Missing legacy values remain unknown until a successful save or fresh disk open.
    #[serde(default)]
    pub saved_encoding: Option<EncodingSpec>,
    #[serde(default)]
    pub saved_line_ending: Option<LineEnding>,
    #[serde(default)]
    pub language_override: Option<String>,
    pub readonly_reason: Option<ReadOnlyReason>,
    pub paged: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewState {
    pub cursor_byte: u64,
    pub scroll_byte: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabLayoutRecord {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub meta: DocMeta,
    pub view: ViewState,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionTabStub {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub durable_rev: Revision,
    pub meta: DocMeta,
    pub view: ViewState,
    pub has_checkpoint: bool,
}
#[derive(Clone, Debug, Default)]
pub struct SessionManifest {
    pub tabs: Vec<SessionTabStub>,
    pub active_doc: Option<DocumentId>,
    pub warnings: Vec<RecoveryWarning>,
}
#[derive(Clone, Debug)]
pub struct RecoveryWarning {
    pub doc_id: Option<DocumentId>,
    pub message: String,
}
#[derive(Clone, Debug)]
pub struct HydratedDraft {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub rope: Rope,
    pub durable_rev: Revision,
    pub identity: ContentIdentity,
    pub meta: DocMeta,
    pub disk_conflict: bool,
}
/// Sequential byte edits. Each range addresses the result of the preceding edit.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextDelta {
    pub byte_range: Range<u64>,
    pub replacement: String,
}
#[derive(Clone, Debug)]
pub struct RecoveryCommit {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub base_rev: Revision,
    pub new_rev: Revision,
    pub rope: Rope,
    pub deltas: Vec<TextDelta>,
    pub meta: DocMeta,
}
#[derive(Clone, Debug)]
pub enum RecoveryEvent {
    DurableAck {
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        durable_rev: Revision,
    },
    StorageError {
        doc_id: Option<DocumentId>,
        error: ServiceError,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    Io,
    InvalidInput,
    Unsupported,
    Busy,
    Cancelled,
    SourceChanged,
    Conflict,
    Storage,
    Corrupt,
    Stale,
    Injected,
    PublishedNotDurable,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceError {
    pub kind: ErrorKind,
    pub message: String,
}
impl ServiceError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
    pub(crate) fn busy() -> Self {
        Self::new(
            ErrorKind::Busy,
            "service queue is full; retain the snapshot and retry",
        )
    }
}
impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}
impl std::error::Error for ServiceError {}
impl From<std::io::Error> for ServiceError {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}
impl From<rusqlite::Error> for ServiceError {
    fn from(e: rusqlite::Error) -> Self {
        Self::new(ErrorKind::Storage, e.to_string())
    }
}
impl From<serde_json::Error> for ServiceError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Corrupt, e.to_string())
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);
impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub(crate) fn check(&self) -> Result<(), ServiceError> {
        if self.is_cancelled() {
            Err(ServiceError::new(
                ErrorKind::Cancelled,
                "operation cancelled",
            ))
        } else {
            Ok(())
        }
    }
}

/// Deterministic one-shot fault hooks shared by tests, CLI, and production workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FaultPoint {
    RecoveryBeforeCommit = 1,
    SaveBeforeWrite = 2,
    SaveAfterTempSync = 3,
    SaveBeforeRename = 4,
    SaveAfterRename = 5,
    SaveBeforeRecoveryAck = 6,
    UiShellBeforePublish = 7,
}
#[derive(Clone, Debug, Default)]
pub struct FaultInjector(Arc<std::sync::atomic::AtomicU8>);
impl FaultInjector {
    pub fn arm(&self, point: FaultPoint) {
        self.0.store(point as u8, Ordering::Release);
    }
    /// Test-process crash simulation: exits with code 86 without running Rust destructors.
    /// Never arm this in the interactive application.
    pub fn arm_process_exit(&self, point: FaultPoint) {
        self.0.store((point as u8) | 0x80, Ordering::Release);
    }
    pub(crate) fn check(&self, point: FaultPoint) -> Result<(), ServiceError> {
        let armed = self.0.load(Ordering::Acquire);
        if armed & 0x7f == point as u8
            && self
                .0
                .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            if armed & 0x80 != 0 {
                std::process::exit(86);
            }
            Err(ServiceError::new(
                ErrorKind::Injected,
                format!("injected at {point:?}"),
            ))
        } else {
            Ok(())
        }
    }
}
