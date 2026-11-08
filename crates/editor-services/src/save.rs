use crate::{
    ContentIdentity, DocumentId, EncodingSpec, ErrorKind, FaultInjector, FaultPoint,
    FileFingerprint, LifecycleGen, LineEnding, RecoveryHandle, Revision, SaveIntent, SaveStage,
    ServiceError,
};
use ropey::Rope;
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct SaveRequest {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub captured_rev: Revision,
    pub rope: Rope,
    pub target_path: PathBuf,
    pub encoding: EncodingSpec,
    pub line_ending: LineEnding,
    pub expected_fingerprint: Option<FileFingerprint>,
    pub force_overwrite: bool,
}
#[derive(Clone, Debug)]
pub enum SaveOutcome {
    Saved {
        doc_id: DocumentId,
        captured_rev: Revision,
        captured_identity: ContentIdentity,
        fingerprint: FileFingerprint,
    },
    SavedWithRecoveryWarning {
        doc_id: DocumentId,
        captured_rev: Revision,
        captured_identity: ContentIdentity,
        fingerprint: FileFingerprint,
        warning: ServiceError,
    },
    /// Rename succeeded, but a later sync failed. Do not report this as "nothing saved".
    PublishedNotDurable {
        doc_id: DocumentId,
        captured_rev: Revision,
        captured_identity: ContentIdentity,
        fingerprint: Option<FileFingerprint>,
        warning: ServiceError,
    },
    ConflictDetected {
        doc_id: DocumentId,
        current_disk: Option<FileFingerprint>,
    },
    Failed {
        doc_id: DocumentId,
        error: ServiceError,
    },
}
struct TempFile {
    path: PathBuf,
    file: File,
    published: bool,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Blocking worker operation. The captured rope is immutable and independent of future edits.
/// Only macOS publication is a supported product path. Windows is deliberately not claimed.
pub fn save_document(
    request: &SaveRequest,
    recovery: Option<&RecoveryHandle>,
    faults: &FaultInjector,
) -> SaveOutcome {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (recovery, faults);
        SaveOutcome::Failed {
            doc_id: request.doc_id,
            error: ServiceError::new(
                ErrorKind::Unsupported,
                "atomic save is implemented and supported for macOS only",
            ),
        }
    }
    #[cfg(target_os = "macos")]
    {
        match save_macos(request, recovery, faults, false) {
            Ok(outcome) => outcome,
            Err(error) => SaveOutcome::Failed {
                doc_id: request.doc_id,
                error,
            },
        }
    }
}

/// Atomic, non-associating publication. Does not alter recovery metadata or the source draft.
/// Like `save_document`, this is a blocking operation for a background executor.
pub fn save_document_copy(request: &SaveRequest, faults: &FaultInjector) -> SaveOutcome {
    #[cfg(target_os = "macos")]
    {
        match save_macos(request, None, faults, true) {
            Ok(outcome) => outcome,
            Err(error) => SaveOutcome::Failed {
                doc_id: request.doc_id,
                error,
            },
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        save_document(request, None, faults)
    }
}
#[cfg(target_os = "macos")]
fn save_macos(
    request: &SaveRequest,
    recovery: Option<&RecoveryHandle>,
    faults: &FaultInjector,
    copy_only: bool,
) -> Result<SaveOutcome, ServiceError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    static SAVE_SERIALIZER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _save_guard = SAVE_SERIALIZER
        .lock()
        .map_err(|_| ServiceError::new(ErrorKind::Io, "save worker mutex poisoned"))?;
    if request.encoding == EncodingSpec::UnsupportedReadOnly
        || request.line_ending == LineEnding::MixedReadOnly
    {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "unsupported encoding or mixed line endings: read-only; no implicit conversion",
        ));
    }
    let requested_path = std::path::absolute(&request.target_path)?;
    let (target, before) = resolve_target(&requested_path)?;
    if !request.force_overwrite && before != request.expected_fingerprint {
        return Ok(SaveOutcome::ConflictDetected {
            doc_id: request.doc_id,
            current_disk: before,
        });
    }
    let original = if before.is_some() {
        let file = File::open(&target)?;
        let meta = file.metadata()?;
        if Some(&FileFingerprint::of_metadata(&meta)?) != before.as_ref() {
            return Ok(SaveOutcome::ConflictDetected {
                doc_id: request.doc_id,
                current_disk: FileFingerprint::of_path(&target).ok(),
            });
        }
        if meta.nlink() > 1 {
            return Err(ServiceError::new(
                ErrorKind::Unsupported,
                "atomic replacement would break hard links; choose a new Save As path",
            ));
        }
        if meta.permissions().readonly() {
            return Err(ServiceError::new(
                ErrorKind::Io,
                "target is read-only; choose a writable Save As path",
            ));
        }
        Some(file)
    } else {
        None
    };
    let parent = target.parent().ok_or_else(|| {
        ServiceError::new(
            ErrorKind::InvalidInput,
            "save target has no parent directory",
        )
    })?;
    let directory = File::open(parent)?;
    let temp_path = parent.join(format!(".editor-save-{}.tmp", uuid::Uuid::new_v4()));
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp_path)?;
    let mut temp = TempFile {
        path: temp_path,
        file,
        published: false,
    };
    if let Some(original) = &original {
        copy_metadata(original, &temp.file)?;
    }
    let identity = ContentIdentity::of_rope(&request.rope);
    let mut intent = SaveIntent {
        doc_id: request.doc_id,
        lifecycle_gen: request.lifecycle_gen,
        captured_rev: request.captured_rev,
        target_path: target.clone(),
        captured_identity: identity,
        captured_encoding: Some(request.encoding),
        captured_line_ending: Some(request.line_ending),
        stage: SaveStage::Started,
        output_fingerprint: None,
    };
    let mut recovery_warning = if copy_only {
        None
    } else {
        record_intent(recovery, &intent).err()
    };
    faults.check(FaultPoint::SaveBeforeWrite)?;
    {
        let mut writer = BufWriter::with_capacity(256 * 1024, &temp.file);
        encode_rope(
            &request.rope,
            request.encoding,
            request.line_ending,
            &mut writer,
        )?;
        writer.flush()?;
    }
    full_sync(&temp.file)?;
    faults.check(FaultPoint::SaveAfterTempSync)?;
    intent.stage = SaveStage::Prepared;
    intent.output_fingerprint = Some(FileFingerprint::of_metadata(&temp.file.metadata()?)?);
    if !copy_only {
        if let Err(error) = record_intent(recovery, &intent) {
            recovery_warning = Some(error);
        }
    }
    // Resolve the original path again: a retargeted symlink must not redirect publication.
    let (now_target, now_fingerprint) = resolve_target(&requested_path)?;
    if now_target != target || now_fingerprint != before {
        return Ok(SaveOutcome::ConflictDetected {
            doc_id: request.doc_id,
            current_disk: now_fingerprint,
        });
    }
    faults.check(FaultPoint::SaveBeforeRename)?;
    publish(&temp.path, &target, before.is_none())?;
    temp.published = true;
    let durability = faults
        .check(FaultPoint::SaveAfterRename)
        .and_then(|_| directory.sync_all().map_err(ServiceError::from))
        .and_then(|_| full_sync(&temp.file));
    let fingerprint = FileFingerprint::of_path(&target);
    if let Err(error) = durability {
        return Ok(SaveOutcome::PublishedNotDurable {
            doc_id: request.doc_id,
            captured_rev: request.captured_rev,
            captured_identity: identity,
            fingerprint: fingerprint.ok(),
            warning: ServiceError::new(
                ErrorKind::PublishedNotDurable,
                format!("target published; durability not confirmed: {error}"),
            ),
        });
    }
    let fingerprint = match fingerprint {
        Ok(value) => value,
        Err(error) => {
            return Ok(SaveOutcome::PublishedNotDurable {
                doc_id: request.doc_id,
                captured_rev: request.captured_rev,
                captured_identity: identity,
                fingerprint: None,
                warning: error,
            });
        }
    };
    let published_descriptor = temp
        .file
        .metadata()
        .map_err(ServiceError::from)
        .and_then(|meta| FileFingerprint::of_metadata(&meta));
    if published_descriptor.as_ref().ok() != Some(&fingerprint) {
        return Ok(SaveOutcome::PublishedNotDurable {
            doc_id: request.doc_id,
            captured_rev: request.captured_rev,
            captured_identity: identity,
            fingerprint: None,
            warning: ServiceError::new(
                ErrorKind::SourceChanged,
                "target changed again after publication; captured bytes were published but the current path is not confirmed",
            ),
        });
    }
    intent.stage = SaveStage::Published;
    intent.output_fingerprint = Some(fingerprint.clone());
    if !copy_only {
        if let Err(error) = faults
            .check(FaultPoint::SaveBeforeRecoveryAck)
            .and_then(|_| record_intent(recovery, &intent))
        {
            recovery_warning = Some(error);
        }
    }
    Ok(if let Some(warning) = recovery_warning {
        SaveOutcome::SavedWithRecoveryWarning {
            doc_id: request.doc_id,
            captured_rev: request.captured_rev,
            captured_identity: identity,
            fingerprint,
            warning,
        }
    } else {
        SaveOutcome::Saved {
            doc_id: request.doc_id,
            captured_rev: request.captured_rev,
            captured_identity: identity,
            fingerprint,
        }
    })
}
fn record_intent(
    recovery: Option<&RecoveryHandle>,
    intent: &SaveIntent,
) -> Result<(), ServiceError> {
    let recovery = recovery.ok_or_else(|| {
        ServiceError::new(
            ErrorKind::Storage,
            "file saved without recovery intent tracking; recovery service unavailable",
        )
    })?;
    recovery
        .record_save_intent(intent.clone())?
        .recv_timeout(Duration::from_secs(30))
        .map_err(|e| {
            ServiceError::new(
                ErrorKind::Storage,
                format!("save intent acknowledgement unavailable: {e}"),
            )
        })?
}
#[cfg(target_os = "macos")]
fn resolve_target(path: &Path) -> Result<(PathBuf, Option<FileFingerprint>), ServiceError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let target = std::fs::canonicalize(path)?;
            let fingerprint = FileFingerprint::of_path(&target)?;
            Ok((target, Some(fingerprint)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| ServiceError::new(ErrorKind::InvalidInput, "invalid target path"))?;
            let name = path.file_name().ok_or_else(|| {
                ServiceError::new(ErrorKind::InvalidInput, "target must name a file")
            })?;
            Ok((std::fs::canonicalize(parent)?.join(name), None))
        }
        Err(error) => Err(error.into()),
    }
}
#[cfg(target_os = "macos")]
fn copy_metadata(from: &File, to: &File) -> Result<(), ServiceError> {
    use std::os::fd::AsRawFd;
    // SAFETY: both descriptors are live for this call. Null state asks Darwin to allocate
    // and free its internal copy state; METADATA excludes source data copying.
    let result = unsafe {
        libc::fcopyfile(
            from.as_raw_fd(),
            to.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_METADATA,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn full_sync(file: &File) -> Result<(), ServiceError> {
    use std::os::fd::AsRawFd;
    file.sync_all()?;
    // SAFETY: F_FULLFSYNC takes no third argument and the descriptor is live.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(target_os = "macos")]
fn publish(temp: &Path, target: &Path, exclusive: bool) -> Result<(), ServiceError> {
    use std::os::unix::ffi::OsStrExt;
    let temp = std::ffi::CString::new(temp.as_os_str().as_bytes())
        .map_err(|_| ServiceError::new(ErrorKind::InvalidInput, "NUL in temporary path"))?;
    let target = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|_| ServiceError::new(ErrorKind::InvalidInput, "NUL in target path"))?;
    // SAFETY: both C strings are NUL-terminated and remain alive throughout the call.
    // RENAME_EXCL closes the create-new race. Existing-file checks are advisory: POSIX
    // has no compare-and-swap rename against an external writer's expected inode.
    let result = unsafe {
        libc::renamex_np(
            temp.as_ptr(),
            target.as_ptr(),
            if exclusive { libc::RENAME_EXCL } else { 0 },
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
pub(crate) fn encode_rope(
    rope: &Rope,
    encoding: EncodingSpec,
    ending: LineEnding,
    writer: &mut impl Write,
) -> Result<(), ServiceError> {
    if encoding == EncodingSpec::UnsupportedReadOnly || ending == LineEnding::MixedReadOnly {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "unsupported text serialization",
        ));
    }
    if encoding == EncodingSpec::Utf8Bom {
        writer.write_all(&[0xef, 0xbb, 0xbf])?;
    }
    let eol: &[u8] = if ending == LineEnding::Crlf {
        b"\r\n"
    } else {
        b"\n"
    };
    let mut pending_cr = false;
    for chunk in rope.chunks() {
        let bytes = chunk.as_bytes();
        let mut from = 0;
        if pending_cr {
            if bytes.first() != Some(&b'\n') {
                return Err(ServiceError::new(
                    ErrorKind::Unsupported,
                    "isolated CR cannot be saved as a uniform LF/CRLF document",
                ));
            }
            writer.write_all(eol)?;
            from = 1;
            pending_cr = false;
        }
        while from < bytes.len() {
            let Some(relative) = memchr::memchr2(b'\r', b'\n', &bytes[from..]) else {
                break;
            };
            let index = from + relative;
            writer.write_all(&bytes[from..index])?;
            if bytes[index] == b'\r' {
                if index + 1 == bytes.len() {
                    pending_cr = true;
                    from = bytes.len();
                    break;
                }
                if bytes[index + 1] != b'\n' {
                    return Err(ServiceError::new(
                        ErrorKind::Unsupported,
                        "isolated CR cannot be saved as a uniform LF/CRLF document",
                    ));
                }
                from = index + 2;
            } else {
                from = index + 1;
            }
            writer.write_all(eol)?;
        }
        writer.write_all(&bytes[from..])?;
    }
    if pending_cr {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "isolated trailing CR cannot be saved without conversion",
        ));
    }
    Ok(())
}
