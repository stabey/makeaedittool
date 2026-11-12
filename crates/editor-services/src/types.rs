use ropey::Rope;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    fs::File,
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
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
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
        #[cfg(not(windows))]
        {
            Self::of_metadata(&std::fs::metadata(path)?)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            let meta = std::fs::metadata(path)?;
            if !meta.is_file() {
                return Self::of_metadata(&meta);
            }
            match std::fs::OpenOptions::new()
                .access_mode(0)
                .share_mode(7)
                .open(path)
            {
                Ok(file) => Self::of_file(&file),
                Err(_) => Self::of_metadata(&meta),
            }
        }
    }
    pub fn of_file(file: &File) -> Result<Self, ServiceError> {
        #[cfg(not(windows))]
        {
            Self::of_metadata(&file.metadata()?)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            if !file.metadata()?.is_file() {
                return Err(ServiceError::new(
                    ErrorKind::Unsupported,
                    "not a regular file",
                ));
            }
            let info = windows_fs::file_information(file)?;
            let mut flags = 0;
            // SAFETY: the file handle is live, flags is writable, and all optional
            // output buffers are null with zero lengths.
            let volume_known = unsafe {
                windows_fs::GetVolumeInformationByHandleW(
                    file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut flags,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0;
            let file_index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
            let (dev, ino) = if volume_known
                && flags & windows_fs::FILE_SUPPORTS_OPEN_BY_FILE_ID != 0
                && file_index != 0
                && file_index != u64::MAX
            {
                (info.dwVolumeSerialNumber as u64, file_index)
            } else {
                (0, 0)
            };
            let ticks =
                ((info.ftLastWriteTime.high as u64) << 32) | info.ftLastWriteTime.low as u64;
            Ok(Self {
                len: ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64,
                mtime_ns: (ticks as i128 - 116_444_736_000_000_000) * 100,
                ctime_ns: 0,
                dev,
                ino,
            })
        }
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

#[cfg(windows)]
pub(crate) mod windows_fs {
    use super::ServiceError;
    use std::{ffi::c_void, fs::File, os::windows::io::AsRawHandle, path::Path};

    pub(crate) const FILE_SUPPORTS_OPEN_BY_FILE_ID: u32 = 0x0100_0000;
    pub(crate) const FILE_ATTRIBUTE_READONLY: u32 = 0x0000_0001;
    pub(crate) const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    pub(crate) const FILE_ATTRIBUTE_SYSTEM: u32 = 0x0000_0004;
    pub(crate) const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    pub(crate) const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x0000_0020;
    pub(crate) const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    pub(crate) const FILE_ATTRIBUTE_NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
    pub(crate) const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
    pub(crate) const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(crate) struct FileTime {
        pub(crate) low: u32,
        pub(crate) high: u32,
    }

    #[repr(C)]
    #[allow(non_camel_case_types, non_snake_case)]
    pub(crate) struct BY_HANDLE_FILE_INFORMATION {
        pub(crate) dwFileAttributes: u32,
        pub(crate) ftCreationTime: FileTime,
        pub(crate) ftLastAccessTime: FileTime,
        pub(crate) ftLastWriteTime: FileTime,
        pub(crate) dwVolumeSerialNumber: u32,
        pub(crate) nFileSizeHigh: u32,
        pub(crate) nFileSizeLow: u32,
        pub(crate) nNumberOfLinks: u32,
        pub(crate) nFileIndexHigh: u32,
        pub(crate) nFileIndexLow: u32,
    }
    const _: () = assert!(std::mem::size_of::<BY_HANDLE_FILE_INFORMATION>() == 52);

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub(crate) fn GetFileInformationByHandle(
            file: *mut c_void,
            information: *mut BY_HANDLE_FILE_INFORMATION,
        ) -> i32;
        pub(crate) fn GetVolumeInformationByHandleW(
            file: *mut c_void,
            volume_name: *mut u16,
            volume_name_len: u32,
            serial_number: *mut u32,
            max_component_len: *mut u32,
            flags: *mut u32,
            filesystem_name: *mut u16,
            filesystem_name_len: u32,
        ) -> i32;
        pub(crate) fn MoveFileExW(existing: *const u16, target: *const u16, flags: u32) -> i32;
        #[allow(dead_code)]
        pub(crate) fn GetFileAttributesW(path: *const u16) -> u32;
        pub(crate) fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }

    pub(crate) fn file_information(
        file: &File,
    ) -> Result<BY_HANDLE_FILE_INFORMATION, ServiceError> {
        let mut information = std::mem::MaybeUninit::uninit();
        // SAFETY: the handle is live and the output points to a correctly sized,
        // aligned BY_HANDLE_FILE_INFORMATION. Success initializes every field.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) }
            == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: GetFileInformationByHandle succeeded above.
        Ok(unsafe { information.assume_init() })
    }

    pub(crate) fn wide_path(path: &Path) -> Result<Vec<u16>, ServiceError> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        if wide[..wide.len() - 1].contains(&0) {
            return Err(ServiceError::new(
                super::ErrorKind::InvalidInput,
                "NUL in path",
            ));
        }
        Ok(wide)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    pub cursor_byte: u64,
    pub scroll_byte: u64,
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
