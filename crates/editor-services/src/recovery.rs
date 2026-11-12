use crate::session_layout::*;
use crate::{
    CancelToken, ContentIdentity, DocMeta, DocumentId, EncodingSpec, ErrorKind, FaultInjector,
    FaultPoint, FileFingerprint, HydratedDraft, LifecycleGen, LineEnding, RecoveryCommit,
    RecoveryEvent, Revision, ServiceError, TextDelta, ViewState,
};
#[path = "recovery_layout.rs"]
mod layout_store;
use crossbeam_channel::{Receiver, Sender, bounded};
use ropey::{Rope, RopeBuilder};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const RETAINED_LIMIT: usize = 128 * 1024 * 1024;
const DOC_LIMIT: usize = 64 * 1024 * 1024;
const CHUNK_BYTES: usize = 256 * 1024;
const PROMOTE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOCS: usize = 128;
pub type Reply<T> = Receiver<Result<T, ServiceError>>;

fn read_only_snapshot(database: impl AsRef<Path>) -> Result<Connection, ServiceError> {
    let conn = Connection::open_with_flags(
        database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.execute_batch("BEGIN DEFERRED")?;
    Ok(conn)
}

/// Reconcile an accepted command after its worker disconnects, without creating a
/// second writer or mutating recovery data. Call off the UI thread.
pub fn inspect_recovery_session(
    database: impl AsRef<Path>,
) -> Result<(SessionManifest, ClosedViewPage), ServiceError> {
    let conn = read_only_snapshot(database)?;
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 2 {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "read-only reconciliation requires schema version 2",
        ));
    }
    Ok((
        layout_store::read_only_manifest(&conn)?,
        layout_store::recently_closed(&conn, None, 64)?,
    ))
}

#[derive(Debug)]
pub enum StartupRecoveryState {
    PreImage,
    AfterImage {
        manifest: SessionManifest,
        closed: ClosedViewPage,
    },
}
/// Inspect a disconnected startup without opening a replacement writer.
pub fn inspect_recovery_startup(
    database: impl AsRef<Path>,
) -> Result<StartupRecoveryState, ServiceError> {
    let path = database.as_ref();
    if !path.try_exists()? {
        return Ok(StartupRecoveryState::PreImage);
    }
    let conn = read_only_snapshot(path)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let names = conn
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<_>, _>>()?;
    let base = [
        "documents",
        "recovery_heads",
        "checkpoint_chunks",
        "edit_transactions",
        "save_intents",
    ];
    let layout = [
        "layout_view_sets",
        "layout_states",
        "layout_views",
        "layout_heads",
        "preserved_layouts",
        "closed_views",
    ];
    if version == 0 && names.is_empty() {
        return Ok(StartupRecoveryState::PreImage);
    }
    if version == 1
        && base.iter().all(|name| names.contains(*name))
        && names.contains("windows_views")
        && names.contains("session")
        && layout.iter().all(|name| !names.contains(*name))
        && !names.contains("legacy_v1_windows_views")
        && !names.contains("legacy_v1_session")
    {
        return Ok(StartupRecoveryState::PreImage);
    }
    let archive_pair =
        names.contains("legacy_v1_windows_views") == names.contains("legacy_v1_session");
    if version == 2
        && base
            .iter()
            .chain(layout.iter())
            .all(|name| names.contains(*name))
        && archive_pair
        && !names.contains("windows_views")
        && !names.contains("session")
    {
        return Ok(StartupRecoveryState::AfterImage {
            manifest: layout_store::read_only_manifest(&conn)?,
            closed: layout_store::recently_closed(&conn, None, 64)?,
        });
    }
    Err(ServiceError::new(
        ErrorKind::Corrupt,
        "startup schema is mixed or unknown; read-only inspection cannot prove either image",
    ))
}
/// A true result proves the complete final text barrier. False proves unchanged
/// live bindings with at least one undurable revision; neither result is an ACK.
pub fn inspect_recovery_shutdown(
    database: impl AsRef<Path>,
    expected: &SessionLayout,
    required: &[(DocumentKey, Revision)],
) -> Result<bool, ServiceError> {
    let (manifest, _) = inspect_recovery_session(database)?;
    if manifest.layout != *expected {
        return Err(ServiceError::new(
            ErrorKind::Stale,
            "shutdown layout identity differs from the frozen session",
        ));
    }
    let mut durable = true;
    for (key, revision) in required {
        let stub = manifest
            .documents
            .iter()
            .find(|stub| stub.key() == *key)
            .ok_or_else(|| {
                ServiceError::new(
                    ErrorKind::Stale,
                    "shutdown document identity cannot be reconciled",
                )
            })?;
        durable &= stub.has_checkpoint && stub.durable_rev >= *revision;
    }
    Ok(durable)
}
/// Read-only rescue access after a proven startup after-image with a dead worker.
pub fn read_recovery_page(
    database: impl AsRef<Path>,
    request: RecoveryPageRequest,
) -> Result<RecoveryPage, ServiceError> {
    let conn = read_only_snapshot(database)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 2 {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "read-only catalog requires schema version 2",
        ));
    }
    layout_store::recovery_page(&conn, request)
}
pub fn read_recovery_draft(
    database: impl AsRef<Path>,
    key: DocumentKey,
    cancel: &CancelToken,
) -> Result<HydratedDraft, ServiceError> {
    let conn = read_only_snapshot(database)?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 2 {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "read-only hydration requires schema version 2",
        ));
    }
    hydrate(&conn, key, cancel)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaveIntent {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub captured_rev: Revision,
    pub target_path: PathBuf,
    pub captured_identity: ContentIdentity,
    #[serde(default)]
    pub captured_encoding: Option<EncodingSpec>,
    #[serde(default)]
    pub captured_line_ending: Option<LineEnding>,
    pub stage: SaveStage,
    pub output_fingerprint: Option<FileFingerprint>,
}
impl SaveIntent {
    /// Worker-only crash reconciliation. Rename may change ctime, so stable
    /// file identity, length, and mtime are compared. Volumes without file IDs
    /// fail closed. This is not proof against arbitrary writers. Never republishes.
    pub fn target_matches_prepared_output(&self) -> Result<bool, ServiceError> {
        let Some(expected) = &self.output_fingerprint else {
            return Ok(false);
        };
        if expected.dev == 0 || expected.ino == 0 {
            return Ok(false);
        }
        let current = match FileFingerprint::of_path(&self.target_path) {
            Ok(current) => current,
            Err(error) => {
                // ServiceError deliberately has no platform-specific IO fields.
                // Only on the error path, distinguish a missing target from an
                // inaccessible or unsupported one without weakening fingerprinting.
                if std::fs::metadata(&self.target_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    return Ok(false);
                }
                return Err(error);
            }
        };
        Ok(current.dev == expected.dev
            && current.ino == expected.ino
            && current.len == expected.len
            && current.mtime_ns == expected.mtime_ns)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SaveStage {
    Started,
    Prepared,
    Published,
}
#[derive(Clone)]
pub struct RecoveryHandle {
    inner: Arc<HandleInner>,
}
struct HandleInner {
    commands: Sender<Command>,
    events: Receiver<RecoveryEvent>,
    mailbox: Arc<Mutex<Mailbox>>,
    last_error: Arc<Mutex<Option<ServiceError>>>,
    faults: FaultInjector,
}
struct Mailbox {
    pending: HashMap<DocumentId, PendingCommit>,
    retained_bytes: usize,
    inflight_bytes: usize,
    accepting: bool,
    manifest_available: bool,
    next_meta_seq: u64,
    // MAX_DOCS pending commits, one in-flight commit, and 33 accepted layouts
    // (32 queued + one executing), each <=1024 entries. One tentative layout
    // admission may also hold refs until try_send rollback, all under this lock.
    // Completed sources prune stamps; no ever-opened-document history is retained.
    meta_state: HashMap<DocumentId, MetaState>,
}
struct PendingCommit {
    commit: RecoveryCommit,
    meta_seq: u64,
}
#[derive(Default)]
struct MetaState {
    outstanding: u32,
    applied: Option<(LifecycleGen, u64)>,
}
impl Mailbox {
    fn next_meta_seq(&mut self) -> Result<u64, ServiceError> {
        self.next_meta_seq = self.next_meta_seq.checked_add(1).ok_or_else(|| {
            ServiceError::new(ErrorKind::Storage, "metadata admission sequence exhausted")
        })?;
        Ok(self.next_meta_seq)
    }

    fn retain_meta(&mut self, doc: DocumentId) {
        self.meta_state.entry(doc).or_default().outstanding += 1;
    }

    fn release_meta(&mut self, doc: DocumentId) {
        if let Some(state) = self.meta_state.get_mut(&doc) {
            state.outstanding -= 1;
            if state.outstanding == 0 {
                self.meta_state.remove(&doc);
            }
        }
    }

    fn record_meta_applied(&mut self, doc: DocumentId, gen_: LifecycleGen, seq: u64) {
        if let Some(state) = self.meta_state.get_mut(&doc) {
            if state.applied.is_none_or(|previous| (gen_, seq) > previous) {
                state.applied = Some((gen_, seq));
            }
        }
    }

    fn has_newer_meta(&self, doc: DocumentId, gen_: LifecycleGen, seq: u64) -> bool {
        self.meta_state
            .get(&doc)
            .and_then(|state| state.applied)
            .is_some_and(|(applied_gen, applied_seq)| applied_gen == gen_ && applied_seq > seq)
    }
}
enum Command {
    Wake,
    Manifest {
        reply: Sender<Result<SessionManifest, ServiceError>>,
    },
    RecentlyClosed {
        before: Option<u64>,
        limit: u16,
        reply: Sender<Result<ClosedViewPage, ServiceError>>,
    },
    RecoveryPage {
        request: RecoveryPageRequest,
        reply: Sender<Result<RecoveryPage, ServiceError>>,
    },
    Hydrate {
        key: DocumentKey,
        cancel: CancelToken,
        reply: Sender<Result<HydratedDraft, ServiceError>>,
    },
    Layout {
        write: SessionWrite,
        meta_seq: u64,
        reply: Sender<Result<LayoutAck, ServiceError>>,
    },
    Close {
        batch: CloseBatch,
        reply: Sender<Result<CloseAck, ServiceError>>,
    },
    Replace {
        replacement: ReloadReplacement,
        reply: Sender<Result<LayoutAck, ServiceError>>,
    },
    Flush {
        shutdown: bool,
        reply: Sender<Result<(), ServiceError>>,
    },
    Intent {
        value: SaveIntent,
        reply: Sender<Result<(), ServiceError>>,
    },
    Intents {
        reply: Sender<Result<Vec<SaveIntent>, ServiceError>>,
    },
}
impl RecoveryHandle {
    /// Start on a background executor: schema/open errors are returned before accepting work.
    pub fn start(database: impl AsRef<Path>) -> Result<Self, ServiceError> {
        Self::start_with_faults(database, FaultInjector::default())
    }
    pub fn start_with_faults(
        database: impl AsRef<Path>,
        faults: FaultInjector,
    ) -> Result<Self, ServiceError> {
        let path = database.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let (commands, rx) = bounded(32);
        let (event_tx, events) = bounded(256);
        let (ready_tx, ready_rx) = bounded(1);
        let mailbox = Arc::new(Mutex::new(Mailbox {
            pending: HashMap::new(),
            retained_bytes: 0,
            inflight_bytes: 0,
            accepting: true,
            manifest_available: false,
            next_meta_seq: 0,
            meta_state: HashMap::new(),
        }));
        let last_error = Arc::new(Mutex::new(None));
        let shared = mailbox.clone();
        let errors = last_error.clone();
        let worker_faults = faults.clone();
        std::thread::Builder::new()
            .name("recovery-sqlite".into())
            .spawn(move || {
                let conn = match layout_store::open_database(&path, &worker_faults) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                worker(conn, rx, event_tx, shared, errors, worker_faults);
            })?;
        ready_rx.recv().map_err(|_| {
            ServiceError::new(
                ErrorKind::Storage,
                "startup outcome indeterminate: recovery worker exited before acknowledgement",
            )
        })??;
        Ok(Self {
            inner: Arc::new(HandleInner {
                commands,
                events,
                mailbox,
                last_error,
                faults,
            }),
        })
    }
    pub fn events(&self) -> Receiver<RecoveryEvent> {
        self.inner.events.clone()
    }
    pub fn faults(&self) -> FaultInjector {
        self.inner.faults.clone()
    }
    pub fn last_error(&self) -> Option<ServiceError> {
        self.inner
            .last_error
            .try_lock()
            .ok()
            .and_then(|e| e.clone())
    }
    /// Nonblocking admission. On Busy retain the newest snapshot and retry with empty deltas.
    /// Adjacent commits coalesce by concatenating sequential deltas, never by copying the rope.
    pub fn try_commit(&self, mut commit: RecoveryCommit) -> Result<(), ServiceError> {
        if commit.rope.len() > DOC_LIMIT {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "recovery text exceeds ordinary-document 64 MiB limit",
            ));
        }
        let incoming_deltas = delta_bytes(&commit.deltas);
        if incoming_deltas >= PROMOTE_BYTES || commit.deltas.len() > 4096 {
            commit.deltas = Vec::new();
        }
        let mut box_ = self
            .inner
            .mailbox
            .try_lock()
            .map_err(|_| ServiceError::busy())?;
        if !box_.accepting {
            return Err(ServiceError::new(
                ErrorKind::Storage,
                "recovery is shutting down",
            ));
        }
        if box_.pending.len() >= MAX_DOCS && !box_.pending.contains_key(&commit.doc_id) {
            return Err(ServiceError::busy());
        }
        if let Some(old) = box_.pending.get(&commit.doc_id) {
            let old = &old.commit;
            if commit.lifecycle_gen < old.lifecycle_gen
                || (commit.lifecycle_gen == old.lifecycle_gen && commit.new_rev <= old.new_rev)
            {
                return Err(ServiceError::new(
                    ErrorKind::Stale,
                    "commit regresses queued revision or lifecycle",
                ));
            }
        }
        let old_weight = box_
            .pending
            .get(&commit.doc_id)
            .map(|old| commit_weight(&old.commit))
            .unwrap_or(0);
        let extra_delta = box_
            .pending
            .get(&commit.doc_id)
            .filter(|old| {
                let old = &old.commit;
                old.lifecycle_gen == commit.lifecycle_gen
                    && old.new_rev == commit.base_rev
                    && !old.deltas.is_empty()
                    && !commit.deltas.is_empty()
            })
            .map(|old| delta_bytes(&old.commit.deltas))
            .unwrap_or(0);
        let prospective = commit_weight(&commit).saturating_add(extra_delta);
        if box_.retained_bytes - old_weight + prospective + box_.inflight_bytes > RETAINED_LIMIT {
            return Err(ServiceError::busy());
        }
        let meta_seq = box_.next_meta_seq()?;
        if let Some(old) = box_.pending.remove(&commit.doc_id) {
            let mut old = old.commit;
            box_.retained_bytes -= old_weight;
            if old.lifecycle_gen == commit.lifecycle_gen
                && old.new_rev == commit.base_rev
                && !old.deltas.is_empty()
                && !commit.deltas.is_empty()
                && delta_bytes(&old.deltas) + delta_bytes(&commit.deltas) < PROMOTE_BYTES
                && old.deltas.len() + commit.deltas.len() <= 4096
            {
                old.deltas.append(&mut commit.deltas);
                commit.deltas = old.deltas;
                commit.base_rev = old.base_rev;
            } else {
                commit.deltas = Vec::new();
            }
        } else {
            box_.retain_meta(commit.doc_id);
        }
        box_.retained_bytes += commit_weight(&commit);
        box_.pending
            .insert(commit.doc_id, PendingCommit { commit, meta_seq });
        drop(box_);
        let _ = self.inner.commands.try_send(Command::Wake);
        Ok(())
    }
    pub fn request_manifest(&self) -> Result<Reply<SessionManifest>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Manifest { reply })?;
        Ok(rx)
    }
    pub fn request_recently_closed(
        &self,
        before_close_id: Option<u64>,
        limit: u16,
    ) -> Result<Reply<ClosedViewPage>, ServiceError> {
        layout_store::validate_page_limit(limit)?;
        let (reply, rx) = bounded(1);
        self.send(Command::RecentlyClosed {
            before: before_close_id,
            limit,
            reply,
        })?;
        Ok(rx)
    }
    pub fn request_recovery_page(
        &self,
        request: RecoveryPageRequest,
    ) -> Result<Reply<RecoveryPage>, ServiceError> {
        layout_store::validate_page_limit(request.limit)?;
        let (reply, rx) = bounded(1);
        self.send(Command::RecoveryPage { request, reply })?;
        Ok(rx)
    }
    pub fn request_hydrate(
        &self,
        key: DocumentKey,
        cancel: CancelToken,
    ) -> Result<Reply<HydratedDraft>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Hydrate { key, cancel, reply })?;
        Ok(rx)
    }
    pub fn write_session(&self, write: SessionWrite) -> Result<Reply<LayoutAck>, ServiceError> {
        if let LayoutPayload::Structural(layout) = &write.layout {
            layout.validate()?;
        }
        if matches!(write.layout, LayoutPayload::TreeOnly(_)) && !write.document_changes.is_empty()
        {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "tree-only writes cannot change document metadata",
            ));
        }
        let mut box_ = self
            .inner
            .mailbox
            .try_lock()
            .map_err(|_| ServiceError::busy())?;
        if !box_.accepting {
            return Err(ServiceError::new(
                ErrorKind::Storage,
                "recovery is shutting down",
            ));
        }
        if !box_.manifest_available {
            return Err(ServiceError::new(
                ErrorKind::Stale,
                "initial session manifest has not been admitted",
            ));
        }
        let meta_seq = box_.next_meta_seq()?;
        for change in &write.document_changes {
            box_.retain_meta(change.key().doc_id);
        }
        let (reply, rx) = bounded(1);
        match self.inner.commands.try_send(Command::Layout {
            write,
            meta_seq,
            reply,
        }) {
            Ok(()) => Ok(rx),
            Err(error) => {
                let full = error.is_full();
                if let Command::Layout { write, .. } = error.into_inner() {
                    for change in write.document_changes {
                        box_.release_meta(change.key().doc_id);
                    }
                }
                Err(if full {
                    ServiceError::busy()
                } else {
                    ServiceError::new(ErrorKind::Storage, "recovery worker is offline")
                })
            }
        }
    }
    pub fn close_views(&self, batch: CloseBatch) -> Result<Reply<CloseAck>, ServiceError> {
        batch.after.validate()?;
        let (reply, rx) = bounded(1);
        self.send(Command::Close { batch, reply })?;
        Ok(rx)
    }
    pub fn replace_view_document(
        &self,
        replacement: ReloadReplacement,
    ) -> Result<Reply<LayoutAck>, ServiceError> {
        replacement.after.validate()?;
        let (reply, rx) = bounded(1);
        self.send(Command::Replace { replacement, reply })?;
        Ok(rx)
    }
    pub fn flush(&self) -> Result<Reply<()>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Flush {
            shutdown: false,
            reply,
        })?;
        Ok(rx)
    }
    pub fn shutdown(&self) -> Result<Reply<()>, ServiceError> {
        let (reply, rx) = bounded(1);
        let mut mailbox = self
            .inner
            .mailbox
            .try_lock()
            .map_err(|_| ServiceError::busy())?;
        self.send(Command::Flush {
            shutdown: true,
            reply,
        })?;
        mailbox.accepting = false;
        Ok(rx)
    }
    pub fn record_save_intent(&self, value: SaveIntent) -> Result<Reply<()>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Intent { value, reply })?;
        Ok(rx)
    }
    pub fn request_save_intents(&self) -> Result<Reply<Vec<SaveIntent>>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Intents { reply })?;
        Ok(rx)
    }
    fn send(&self, command: Command) -> Result<(), ServiceError> {
        self.inner.commands.try_send(command).map_err(|e| {
            if e.is_full() {
                ServiceError::busy()
            } else {
                ServiceError::new(ErrorKind::Storage, "recovery worker is offline")
            }
        })
    }
}
fn delta_bytes(deltas: &[TextDelta]) -> usize {
    deltas.iter().map(|d| d.replacement.len() + 64).sum()
}
fn commit_weight(commit: &RecoveryCommit) -> usize {
    commit.rope.len() + delta_bytes(&commit.deltas) + 1024
}
fn sql_rev(value: u64) -> Result<i64, ServiceError> {
    i64::try_from(value).map_err(|_| {
        ServiceError::new(
            ErrorKind::InvalidInput,
            "revision exceeds SQLite signed integer range",
        )
    })
}
#[derive(Default)]
struct SnapshotCache {
    entries: HashMap<DocumentId, (Revision, Rope)>,
    order: VecDeque<DocumentId>,
    bytes: usize,
}
impl SnapshotCache {
    fn put(&mut self, doc: DocumentId, rev: Revision, rope: Rope) {
        if let Some((_, old)) = self.entries.remove(&doc) {
            self.bytes -= old.len();
        }
        self.order.retain(|id| *id != doc);
        while self.bytes + rope.len() > DOC_LIMIT || self.entries.len() >= MAX_DOCS {
            let Some(old_id) = self.order.pop_front() else {
                break;
            };
            if let Some((_, old)) = self.entries.remove(&old_id) {
                self.bytes -= old.len();
            }
        }
        self.bytes += rope.len();
        self.order.push_back(doc);
        self.entries.insert(doc, (rev, rope));
    }
    fn remove(&mut self, doc: DocumentId) {
        if let Some((_, rope)) = self.entries.remove(&doc) {
            self.bytes -= rope.len();
        }
        self.order.retain(|id| *id != doc);
    }
}
fn worker(
    mut conn: Connection,
    commands: Receiver<Command>,
    events: Sender<RecoveryEvent>,
    mailbox: Arc<Mutex<Mailbox>>,
    last_error: Arc<Mutex<Option<ServiceError>>>,
    faults: FaultInjector,
) {
    let mut cache = SnapshotCache::default();
    let mut last_batch = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut pending_error: Option<ServiceError> = None;
    loop {
        let timeout = Duration::from_millis(250).saturating_sub(last_batch.elapsed());
        let command = match commands.recv_timeout(timeout) {
            Ok(command) => Some(command),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let _ = drain(
                    &mut conn,
                    &mailbox,
                    &events,
                    &last_error,
                    &faults,
                    &mut cache,
                );
                break;
            }
        };
        let urgent = mailbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .values()
            .map(|c| delta_bytes(&c.commit.deltas))
            .sum::<usize>()
            >= 1024 * 1024;
        if last_batch.elapsed() >= Duration::from_millis(250) || urgent {
            if let Err(error) = drain(
                &mut conn,
                &mailbox,
                &events,
                &last_error,
                &faults,
                &mut cache,
            ) {
                pending_error = Some(error);
            }
            last_batch = Instant::now();
        }
        match command {
            Some(Command::Wake) | None => {}
            Some(Command::Manifest { reply }) => {
                let result = layout_store::manifest(&mut conn);
                if result.is_ok() {
                    mailbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .manifest_available = true;
                }
                let _ = reply.send(result);
            }
            Some(Command::RecentlyClosed {
                before,
                limit,
                reply,
            }) => {
                let _ = reply.send(layout_store::recently_closed(&conn, before, limit));
            }
            Some(Command::RecoveryPage { request, reply }) => {
                let _ = reply.send(layout_store::recovery_page(&conn, request));
            }
            Some(Command::Hydrate { key, cancel, reply }) => {
                let _ = reply.send(hydrate(&conn, key, &cancel));
            }
            Some(Command::Layout {
                write,
                meta_seq,
                reply,
            }) => {
                let result =
                    layout_store::write_session(&mut conn, &write, meta_seq, &mailbox, &faults);
                let mut box_ = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                for change in &write.document_changes {
                    let key = change.key();
                    if result.is_ok() {
                        box_.record_meta_applied(key.doc_id, key.lifecycle_gen, meta_seq);
                    }
                    box_.release_meta(key.doc_id);
                }
                drop(box_);
                if let Err(error) = &result {
                    report_error(&events, &last_error, None, error.clone());
                }
                let _ = reply.send(result);
            }
            Some(Command::Close { batch, reply }) => {
                let result = drain(
                    &mut conn,
                    &mailbox,
                    &events,
                    &last_error,
                    &faults,
                    &mut cache,
                )
                .and_then(|_| layout_store::close_views(&mut conn, &batch, &faults));
                if result.is_ok() {
                    for entry in &batch.entries {
                        cache.remove(entry.target.document.doc_id);
                    }
                }
                if let Err(error) = &result {
                    report_error(&events, &last_error, None, error.clone());
                }
                let _ = reply.send(result);
            }
            Some(Command::Replace { replacement, reply }) => {
                let result = layout_store::replace_view_document(&mut conn, &replacement, &faults);
                if result.is_ok() {
                    cache.remove(replacement.target.document.doc_id);
                }
                if let Err(error) = &result {
                    report_error(&events, &last_error, None, error.clone());
                }
                let _ = reply.send(result);
            }
            Some(Command::Flush { shutdown, reply }) => {
                let result = drain(
                    &mut conn,
                    &mailbox,
                    &events,
                    &last_error,
                    &faults,
                    &mut cache,
                )
                .and_then(|_| pending_error.take().map_or(Ok(()), Err));
                if shutdown && result.is_ok() {
                    // The acknowledgement is also a handle-release barrier. On
                    // Windows callers may immediately remove the database directory.
                    drop(cache);
                    drop(conn);
                    let _ = reply.send(result);
                    break;
                }
                if shutdown {
                    mailbox.lock().unwrap_or_else(|e| e.into_inner()).accepting = true;
                }
                let _ = reply.send(result);
            }
            Some(Command::Intent { value, reply }) => {
                let _ = reply.send(record_intent(&mut conn, &value, &faults));
            }
            Some(Command::Intents { reply }) => {
                let result = (|| {
                    let mut statement = conn.prepare("SELECT value FROM save_intents")?;
                    let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
                    let mut intents = Vec::new();
                    for value in rows {
                        intents.push(serde_json::from_str(&value?)?);
                    }
                    Ok(intents)
                })();
                let _ = reply.send(result);
            }
        }
        if last_checkpoint.elapsed() >= Duration::from_secs(5) {
            // Passive maintenance never deletes drafts and never blocks on another reader.
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
            last_checkpoint = Instant::now();
        }
    }
}
fn report_error(
    events: &Sender<RecoveryEvent>,
    errors: &Mutex<Option<ServiceError>>,
    doc_id: Option<DocumentId>,
    error: ServiceError,
) {
    *errors.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
    // Dropping an acknowledgement is conservative; never block disk commits on UI polling.
    let _ = events.try_send(RecoveryEvent::StorageError { doc_id, error });
}
fn drain(
    conn: &mut Connection,
    mailbox: &Mutex<Mailbox>,
    events: &Sender<RecoveryEvent>,
    errors: &Mutex<Option<ServiceError>>,
    faults: &FaultInjector,
    cache: &mut SnapshotCache,
) -> Result<(), ServiceError> {
    let mut first_error = None;
    // Capture the finite set at the barrier; incoming edits cannot starve Flush.
    let ids: Vec<DocumentId> = mailbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .keys()
        .copied()
        .collect();
    for id in ids {
        let Some((pending, preserve_meta)) = take_pending(mailbox, id) else {
            continue;
        };
        let lifecycle_gen = pending.commit.lifecycle_gen;
        let result = commit_one(conn, &pending.commit, preserve_meta, faults, cache);
        finish_commit(mailbox, pending, &result);
        match result {
            Ok(rev) => {
                let _ = events.try_send(RecoveryEvent::DurableAck {
                    doc_id: id,
                    lifecycle_gen,
                    durable_rev: rev,
                });
            }
            Err(error) if error.kind == ErrorKind::Stale => {}
            Err(error) => {
                report_error(events, errors, Some(id), error.clone());
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    first_error.map_or(Ok(()), Err)
}
fn take_pending(mailbox: &Mutex<Mailbox>, id: DocumentId) -> Option<(PendingCommit, bool)> {
    let mut box_ = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    let pending = box_.pending.remove(&id)?;
    let weight = commit_weight(&pending.commit);
    box_.retained_bytes -= weight;
    box_.inflight_bytes += weight;
    let preserve_meta = box_.has_newer_meta(id, pending.commit.lifecycle_gen, pending.meta_seq);
    Some((pending, preserve_meta))
}

fn finish_commit(
    mailbox: &Mutex<Mailbox>,
    mut pending: PendingCommit,
    result: &Result<Revision, ServiceError>,
) {
    let mut box_ = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    let id = pending.commit.doc_id;
    box_.inflight_bytes -= commit_weight(&pending.commit);
    match result {
        Ok(_) => {
            box_.record_meta_applied(id, pending.commit.lifecycle_gen, pending.meta_seq);
        }
        Err(error)
            if matches!(
                error.kind,
                ErrorKind::Io | ErrorKind::Storage | ErrorKind::Injected
            ) && !box_.pending.contains_key(&id) =>
        {
            // A retry keeps its original admission stamp and ref, not a newer priority.
            pending.commit.deltas = Vec::new();
            box_.retained_bytes += commit_weight(&pending.commit);
            box_.pending.insert(id, pending);
            return;
        }
        Err(_) => {}
    }
    // A newer pending commit owns its own ref; a superseded in-flight error releases only this one.
    box_.release_meta(id);
}
fn commit_one(
    conn: &mut Connection,
    job: &RecoveryCommit,
    preserve_meta: bool,
    faults: &FaultInjector,
    cache: &mut SnapshotCache,
) -> Result<Revision, ServiceError> {
    let id = job.doc_id.key();
    let gen_ = sql_rev(job.lifecycle_gen.0)?;
    let new_rev = sql_rev(job.new_rev.0)?;
    let base_rev = sql_rev(job.base_rev.0)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<(i64, i64)> = tx
        .query_row("SELECT gen,state FROM documents WHERE id=?1", [&id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?;
    if let Some((generation, state)) = existing {
        if generation != gen_ || state != 0 {
            return Err(ServiceError::new(
                ErrorKind::Stale,
                "discarded or old lifecycle checkpoint rejected",
            ));
        }
    } else {
        tx.execute(
            "INSERT INTO documents(id,gen,state,meta) VALUES(?1,?2,0,?3)",
            params![id, gen_, serde_json::to_string(&job.meta)?],
        )?;
    }
    let head: Option<(i64, i64, i64, i64, i64)> = tx.query_row("SELECT checkpoint_rev,durable_rev,byte_len,delta_bytes,delta_rows FROM recovery_heads WHERE doc=?1", [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    if let Some((_, durable, _, _, _)) = head {
        if new_rev <= durable {
            return Err(ServiceError::new(
                ErrorKind::Stale,
                "durable revision cannot regress",
            ));
        }
    }
    let encoded = if job.deltas.is_empty() {
        Vec::new()
    } else {
        serde_json::to_vec(&job.deltas)?
    };
    let mut can_delta = head.is_some_and(|(_, durable, len, bytes, rows)| {
        durable == base_rev
            && new_rev > durable
            && !encoded.is_empty()
            && encoded.len() < PROMOTE_BYTES
            && bytes + (encoded.len() as i64) < (4 * 1024 * 1024i64).max(len / 8)
            && rows < 511
    }) && cache
        .entries
        .get(&job.doc_id)
        .is_some_and(|(rev, _)| *rev == job.base_rev);
    if can_delta {
        let Some((_, previous)) = cache.entries.get(&job.doc_id) else {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "missing derived snapshot",
            ));
        };
        let replayed = apply_deltas(previous.clone(), &job.deltas)?;
        // This worker-only O(bytes) equality is a correctness gate, not a UI dirty
        // check. A valid but inaccurate delta must never acknowledge different text.
        can_delta = replayed == job.rope;
    }
    let resulting_rope = job.rope.clone();
    if can_delta {
        let checksum = blake3::hash(&encoded);
        tx.execute("INSERT INTO edit_transactions(doc,base_rev,new_rev,deltas,checksum) VALUES(?1,?2,?3,?4,?5)", params![id, base_rev, new_rev, encoded, checksum.as_bytes().as_slice()])?;
        tx.execute("UPDATE recovery_heads SET durable_rev=?2,byte_len=?3,delta_bytes=delta_bytes+?4,delta_rows=delta_rows+1 WHERE doc=?1", params![id, new_rev, resulting_rope.len() as i64, encoded.len() as i64])?;
    } else {
        let hash = write_checkpoint(&tx, &id, new_rev, &resulting_rope)?;
        tx.execute("INSERT INTO recovery_heads(doc,checkpoint_rev,durable_rev,byte_len,checkpoint_hash,delta_bytes,delta_rows) VALUES(?1,?2,?2,?3,?4,0,0) ON CONFLICT(doc) DO UPDATE SET checkpoint_rev=excluded.checkpoint_rev,durable_rev=MAX(recovery_heads.durable_rev,excluded.durable_rev),byte_len=excluded.byte_len,checkpoint_hash=excluded.checkpoint_hash,delta_bytes=0,delta_rows=0", params![id, new_rev, resulting_rope.len() as i64, hash.as_bytes().as_slice()])?;
        tx.execute(
            "DELETE FROM edit_transactions WHERE doc=?1 AND new_rev<=?2",
            params![id, new_rev],
        )?;
        tx.execute(
            "DELETE FROM checkpoint_chunks WHERE doc=?1 AND rev<?2",
            params![id, new_rev],
        )?;
    }
    let meta = meta_for_source(&tx, &id, &job.meta, preserve_meta)?;
    tx.execute(
        "UPDATE documents SET meta=?2 WHERE id=?1",
        params![id, serde_json::to_string(&meta)?],
    )?;
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    cache.put(job.doc_id, job.new_rev, resulting_rope);
    Ok(job.new_rev)
}
fn write_checkpoint(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    rev: i64,
    rope: &Rope,
) -> Result<blake3::Hash, ServiceError> {
    let mut statement = tx.prepare(
        "INSERT INTO checkpoint_chunks(doc,rev,ordinal,content,checksum) VALUES(?1,?2,?3,?4,?5)",
    )?;
    let mut buffer = Vec::with_capacity(CHUNK_BYTES);
    let mut whole = blake3::Hasher::new();
    let mut ordinal = 0i64;
    for chunk in rope.chunks() {
        whole.update(chunk.as_bytes());
        let mut text = chunk;
        while !text.is_empty() {
            let mut take = (CHUNK_BYTES - buffer.len()).min(text.len());
            while !text.is_char_boundary(take) {
                take -= 1;
            }
            if take == 0 {
                let checksum = blake3::hash(&buffer);
                statement.execute(params![
                    id,
                    rev,
                    ordinal,
                    buffer,
                    checksum.as_bytes().as_slice()
                ])?;
                ordinal += 1;
                buffer.clear();
                continue;
            }
            buffer.extend_from_slice(&text.as_bytes()[..take]);
            text = &text[take..];
            if buffer.len() == CHUNK_BYTES {
                let checksum = blake3::hash(&buffer);
                statement.execute(params![
                    id,
                    rev,
                    ordinal,
                    buffer,
                    checksum.as_bytes().as_slice()
                ])?;
                ordinal += 1;
                buffer.clear();
            }
        }
    }
    if !buffer.is_empty() {
        let checksum = blake3::hash(&buffer);
        statement.execute(params![
            id,
            rev,
            ordinal,
            buffer,
            checksum.as_bytes().as_slice()
        ])?;
    }
    Ok(whole.finalize())
}
fn apply_deltas(mut rope: Rope, deltas: &[TextDelta]) -> Result<Rope, ServiceError> {
    for delta in deltas {
        let start = usize::try_from(delta.byte_range.start)
            .map_err(|_| ServiceError::new(ErrorKind::Corrupt, "edit offset overflow"))?;
        let end = usize::try_from(delta.byte_range.end)
            .map_err(|_| ServiceError::new(ErrorKind::Corrupt, "edit offset overflow"))?;
        if start > end
            || end > rope.len()
            || !rope.is_char_boundary(start)
            || !rope.is_char_boundary(end)
        {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "edit range is not a valid UTF-8 boundary",
            ));
        }
        if rope.len() - (end - start) + delta.replacement.len() > DOC_LIMIT {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "replayed text exceeds document limit",
            ));
        }
        rope.try_remove(start..end)
            .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))?;
        rope.try_insert(start, &delta.replacement)
            .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))?;
    }
    Ok(rope)
}
fn parse_id(value: &str) -> Result<DocumentId, ServiceError> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "document identity must be 32 lowercase hexadecimal bytes",
        ));
    }
    u128::from_str_radix(value, 16)
        .map(DocumentId)
        .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))
}
fn hydrate(
    conn: &Connection,
    key: DocumentKey,
    cancel: &CancelToken,
) -> Result<HydratedDraft, ServiceError> {
    cancel.check()?;
    let doc_id = key.doc_id;
    let id = doc_id.key();
    let stub = layout_store::document_stub(conn, &id)?;
    if stub.key() != key {
        return Err(ServiceError::new(
            ErrorKind::Stale,
            "draft lifecycle changed",
        ));
    }
    let state: i64 = conn.query_row("SELECT state FROM documents WHERE id=?1", [&id], |r| {
        r.get(0)
    })?;
    if state == 2 {
        return Err(ServiceError::new(ErrorKind::Stale, "draft was discarded"));
    }
    let generation = sql_rev(stub.lifecycle_gen.0)?;
    let meta = stub.meta;
    let (checkpoint_rev,durable_rev,expected_len,expected_hash):(i64,i64,i64,Option<[u8;32]>)=conn.query_row(
        "SELECT checkpoint_rev,durable_rev,byte_len,CASE WHEN typeof(checkpoint_hash)='blob' AND LENGTH(checkpoint_hash)=32 THEN checkpoint_hash END FROM recovery_heads WHERE doc=?1",
        [&id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get_ref(3)?.as_blob().ok().and_then(|bytes|bytes.try_into().ok()))))?;
    let expected_hash = expected_hash
        .ok_or_else(|| ServiceError::new(ErrorKind::Corrupt, "invalid checkpoint hash"))?;
    if checkpoint_rev < 0
        || durable_rev < checkpoint_rev
        || expected_len < 0
        || expected_len as usize > DOC_LIMIT
    {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "invalid draft revision or length",
        ));
    }
    let mut builder = RopeBuilder::new();
    let mut hasher = blake3::Hasher::new();
    let mut loaded = 0usize;
    let mut statement=conn.prepare("SELECT ordinal,CASE WHEN typeof(content)='blob' AND LENGTH(content)<=?3 THEN content END,CASE WHEN typeof(checksum)='blob' AND LENGTH(checksum)=32 THEN checksum END FROM checkpoint_chunks WHERE doc=?1 AND rev=?2 ORDER BY ordinal")?;
    let mut rows = statement.query(params![id, checkpoint_rev, CHUNK_BYTES as i64])?;
    let mut ordinal = 0i64;
    while let Some(row) = rows.next()? {
        cancel.check()?;
        let index: i64 = row.get(0)?;
        let bytes = row.get_ref(1)?.as_blob().map_err(|_| {
            ServiceError::new(ErrorKind::Corrupt, "invalid or oversized checkpoint chunk")
        })?;
        let checksum = row.get_ref(2)?.as_blob().map_err(|_| {
            ServiceError::new(ErrorKind::Corrupt, "invalid checkpoint chunk checksum")
        })?;
        loaded += bytes.len();
        if index != ordinal
            || bytes.len() > CHUNK_BYTES
            || loaded > DOC_LIMIT
            || blake3::hash(bytes).as_bytes().as_slice() != checksum
        {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "checkpoint chunk checksum/sequence failure",
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|error| ServiceError::new(ErrorKind::Corrupt, error.to_string()))?;
        hasher.update(bytes);
        builder.append(text);
        ordinal += 1;
    }
    if hasher.finalize().as_bytes().as_slice() != expected_hash.as_slice() {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "checkpoint checksum failure",
        ));
    }
    let mut rope = builder.finish();
    let mut rev = checkpoint_rev;
    let mut statement=conn.prepare("SELECT base_rev,new_rev,CASE WHEN typeof(deltas)='blob' AND LENGTH(deltas)<=?3 THEN deltas END,CASE WHEN typeof(checksum)='blob' AND LENGTH(checksum)=32 THEN checksum END FROM edit_transactions WHERE doc=?1 AND new_rev>?2 ORDER BY new_rev")?;
    let mut rows = statement.query(params![id, checkpoint_rev, PROMOTE_BYTES as i64])?;
    while let Some(row) = rows.next()? {
        cancel.check()?;
        let base: i64 = row.get(0)?;
        let next: i64 = row.get(1)?;
        let bytes = row.get_ref(2)?.as_blob().map_err(|_| {
            ServiceError::new(ErrorKind::Corrupt, "invalid or oversized edit transaction")
        })?;
        let checksum = row.get_ref(3)?.as_blob().map_err(|_| {
            ServiceError::new(ErrorKind::Corrupt, "invalid edit transaction checksum")
        })?;
        if base != rev
            || next <= base
            || next > durable_rev
            || bytes.len() > PROMOTE_BYTES
            || blake3::hash(bytes).as_bytes().as_slice() != checksum
        {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "edit log checksum or revision chain failure",
            ));
        }
        let deltas: Vec<TextDelta> = serde_json::from_slice(bytes)?;
        rope = apply_deltas(rope, &deltas)?;
        rev = next;
    }
    if rev != durable_rev || rope.len() as i64 != expected_len {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "recovery head is not fully covered",
        ));
    }
    cancel.check()?;
    let identity = ContentIdentity::of_rope(&rope);
    let disk_conflict = meta.path.as_ref().is_some_and(|path| {
        FileFingerprint::of_path(path).ok().as_ref() != meta.origin_fingerprint.as_ref()
    });
    Ok(HydratedDraft {
        doc_id,
        lifecycle_gen: LifecycleGen(generation as u64),
        rope,
        durable_rev: Revision(durable_rev as u64),
        identity,
        meta,
        disk_conflict,
    })
}
fn meta_for_source(
    conn: &Connection,
    id: &str,
    incoming: &DocMeta,
    preserve_current: bool,
) -> Result<DocMeta, ServiceError> {
    let meta = if preserve_current {
        let json: String =
            conn.query_row("SELECT meta FROM documents WHERE id=?1", [id], |row| {
                row.get(0)
            })?;
        serde_json::from_str(&json)?
    } else {
        incoming.clone()
    };
    merge_saved_association(conn, id, meta)
}
fn merge_saved_association(
    conn: &Connection,
    id: &str,
    mut meta: DocMeta,
) -> Result<DocMeta, ServiceError> {
    let saved: Option<Option<String>> = conn
        .query_row("SELECT saved_meta FROM documents WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(Some(saved)) = saved {
        let saved: SaveIntent = serde_json::from_str(&saved)?;
        meta.path = Some(saved.target_path);
        meta.saved_identity = Some(saved.captured_identity);
        meta.saved_encoding = saved.captured_encoding;
        meta.saved_line_ending = saved.captured_line_ending;
        meta.origin_fingerprint = saved.output_fingerprint;
    }
    Ok(meta)
}
fn record_intent(
    conn: &mut Connection,
    value: &SaveIntent,
    faults: &FaultInjector,
) -> Result<(), ServiceError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = value.doc_id.key();
    let row: Option<(i64, i64, String, i64)> = tx
        .query_row(
            "SELECT gen,state,meta,saved_rev FROM documents WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let revision = sql_rev(value.captured_rev.0)?;
    let generation = sql_rev(value.lifecycle_gen.0)?;
    let mut meta = if let Some((gen_, state, json, saved_rev)) = row {
        if gen_ != generation || state == 2 || revision < saved_rev {
            return Err(ServiceError::new(
                ErrorKind::Stale,
                "save acknowledgement belongs to an obsolete lifecycle or saved revision",
            ));
        }
        serde_json::from_str::<DocMeta>(&json)?
    } else {
        DocMeta {
            encoding: value.captured_encoding.unwrap_or_default(),
            line_ending: value.captured_line_ending.unwrap_or_default(),
            ..DocMeta::default()
        }
    };
    if matches!(value.stage, SaveStage::Published) {
        meta.path = Some(value.target_path.clone());
        meta.saved_identity = Some(value.captured_identity);
        meta.saved_encoding = value.captured_encoding;
        meta.saved_line_ending = value.captured_line_ending;
        meta.origin_fingerprint = value.output_fingerprint.clone();
        tx.execute("INSERT INTO documents(id,gen,state,meta,saved_rev,saved_meta) VALUES(?1,?2,0,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET meta=excluded.meta,saved_rev=excluded.saved_rev,saved_meta=excluded.saved_meta", params![id,generation,serde_json::to_string(&meta)?,revision,serde_json::to_string(value)?])?;
    }
    tx.execute("INSERT INTO save_intents(doc,value) VALUES(?1,?2) ON CONFLICT(doc) DO UPDATE SET value=excluded.value", params![id,serde_json::to_string(value)?])?;
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "recovery_order_tests.rs"]
mod metadata_order_tests;
