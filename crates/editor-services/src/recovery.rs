use crate::{
    ContentIdentity, DocMeta, DocumentId, EncodingSpec, ErrorKind, FaultInjector, FaultPoint,
    FileFingerprint, HydratedDraft, LifecycleGen, LineEnding, RecoveryCommit, RecoveryEvent,
    Revision, ServiceError, SessionManifest, SessionTabStub, TabLayoutRecord, TextDelta, ViewState,
};
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
    /// Worker-only crash reconciliation. Rename may change ctime, so the prepared
    /// inode/device/length/mtime are compared. This is identity evidence, not a
    /// cryptographic proof against arbitrary external writers. Never republishes.
    pub fn target_matches_prepared_output(&self) -> Result<bool, ServiceError> {
        let Some(expected) = &self.output_fingerprint else {
            return Ok(false);
        };
        let metadata = match std::fs::metadata(&self.target_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let current = FileFingerprint::of_metadata(&metadata)?;
        Ok(current.dev == expected.dev
            && current.ino == expected.ino
            && current.len == expected.len
            && current.mtime_ns == expected.mtime_ns)
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
        closed: bool,
        reply: Sender<Result<SessionManifest, ServiceError>>,
    },
    Hydrate {
        doc_id: DocumentId,
        reply: Sender<Result<HydratedDraft, ServiceError>>,
    },
    Layout {
        tabs: Vec<TabLayoutRecord>,
        active: Option<DocumentId>,
        meta_seq: u64,
    },
    Close {
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        discard: bool,
        reply: Sender<Result<(), ServiceError>>,
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
                let mut conn = match open_database(&path) {
                    Ok(conn) => {
                        let _ = ready_tx.send(Ok(()));
                        conn
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                worker(&mut conn, rx, event_tx, shared, errors, worker_faults);
            })?;
        ready_rx.recv().map_err(|_| {
            ServiceError::new(ErrorKind::Storage, "recovery worker exited during startup")
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
        self.manifest(false)
    }
    pub fn request_recently_closed(&self) -> Result<Reply<SessionManifest>, ServiceError> {
        self.manifest(true)
    }
    fn manifest(&self, closed: bool) -> Result<Reply<SessionManifest>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Manifest { closed, reply })?;
        Ok(rx)
    }
    pub fn request_hydrate(
        &self,
        doc_id: DocumentId,
    ) -> Result<Reply<HydratedDraft>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Hydrate { doc_id, reply })?;
        Ok(rx)
    }
    pub fn save_session_layout(
        &self,
        tabs: Vec<TabLayoutRecord>,
        active: Option<DocumentId>,
    ) -> Result<(), ServiceError> {
        if tabs.len() > 1024 {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "session exceeds 1024 tabs",
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
        let meta_seq = box_.next_meta_seq()?;
        for tab in &tabs {
            box_.retain_meta(tab.doc_id);
        }
        match self.inner.commands.try_send(Command::Layout {
            tabs,
            active,
            meta_seq,
        }) {
            Ok(()) => Ok(()),
            Err(error) => {
                let full = error.is_full();
                if let Command::Layout { tabs, .. } = error.into_inner() {
                    for tab in tabs {
                        box_.release_meta(tab.doc_id);
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
    /// For discard pass a strictly newer lifecycle. The acknowledgement is the durable tombstone.
    pub fn close_document(
        &self,
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        discard: bool,
    ) -> Result<Reply<()>, ServiceError> {
        let (reply, rx) = bounded(1);
        self.send(Command::Close {
            doc_id,
            lifecycle_gen,
            discard,
            reply,
        })?;
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
fn open_database(path: &Path) -> Result<Connection, ServiceError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=512; PRAGMA journal_size_limit=16777216;")?;
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > 1 {
        return Err(ServiceError::new(
            ErrorKind::Unsupported,
            "unknown recovery schema; original database left intact",
        ));
    }
    if version == 0 {
        conn.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS documents(id TEXT PRIMARY KEY, gen INTEGER NOT NULL, state INTEGER NOT NULL, meta TEXT NOT NULL, saved_rev INTEGER NOT NULL DEFAULT -1, saved_meta TEXT);
            CREATE TABLE IF NOT EXISTS recovery_heads(doc TEXT PRIMARY KEY REFERENCES documents(id), checkpoint_rev INTEGER NOT NULL, durable_rev INTEGER NOT NULL, byte_len INTEGER NOT NULL, checkpoint_hash BLOB NOT NULL, delta_bytes INTEGER NOT NULL DEFAULT 0, delta_rows INTEGER NOT NULL DEFAULT 0);
            CREATE TABLE IF NOT EXISTS checkpoint_chunks(doc TEXT NOT NULL REFERENCES documents(id), rev INTEGER NOT NULL, ordinal INTEGER NOT NULL, content BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(doc,rev,ordinal));
            CREATE TABLE IF NOT EXISTS edit_transactions(doc TEXT NOT NULL REFERENCES documents(id), base_rev INTEGER NOT NULL, new_rev INTEGER NOT NULL, deltas BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(doc,new_rev));
            CREATE TABLE IF NOT EXISTS windows_views(doc TEXT PRIMARY KEY REFERENCES documents(id), position INTEGER NOT NULL, view TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS session(key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS save_intents(doc TEXT PRIMARY KEY, value TEXT NOT NULL);
            PRAGMA user_version=1; COMMIT;")?;
    }
    Ok(conn)
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
    conn: &mut Connection,
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
                let _ = drain(conn, &mailbox, &events, &last_error, &faults, &mut cache);
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
            if let Err(error) = drain(conn, &mailbox, &events, &last_error, &faults, &mut cache) {
                pending_error = Some(error);
            }
            last_batch = Instant::now();
        }
        match command {
            Some(Command::Wake) | None => {}
            Some(Command::Manifest { closed, reply }) => {
                let _ = reply.send(manifest(conn, closed));
            }
            Some(Command::Hydrate { doc_id, reply }) => {
                let _ = reply.send(hydrate(conn, doc_id));
            }
            Some(Command::Layout {
                tabs,
                active,
                meta_seq,
            }) => {
                if let Err(error) = layout(conn, tabs, active, meta_seq, &mailbox) {
                    report_error(&events, &last_error, None, error.clone());
                    pending_error = Some(error);
                }
            }
            Some(Command::Close {
                doc_id,
                lifecycle_gen,
                discard,
                reply,
            }) => {
                let result = close(conn, doc_id, lifecycle_gen, discard, &faults);
                if result.is_ok() {
                    cache.remove(doc_id);
                    // Close owns no metadata ref. Queued sources retain theirs until
                    // completion (including stale rejection), then prune their stamps.
                } else if let Err(error) = &result {
                    report_error(&events, &last_error, Some(doc_id), error.clone());
                }
                let _ = reply.send(result);
            }
            Some(Command::Flush { shutdown, reply }) => {
                let result = drain(conn, &mailbox, &events, &last_error, &faults, &mut cache)
                    .and_then(|_| pending_error.take().map_or(Ok(()), Err));
                let success = result.is_ok();
                let _ = reply.send(result);
                if shutdown && success {
                    break;
                }
                if shutdown {
                    mailbox.lock().unwrap_or_else(|e| e.into_inner()).accepting = true;
                }
            }
            Some(Command::Intent { value, reply }) => {
                let _ = reply.send(record_intent(conn, &value, &faults));
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
        if generation != gen_ || state == 2 {
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
fn manifest(conn: &Connection, closed: bool) -> Result<SessionManifest, ServiceError> {
    // Deliberately no SELECT of checkpoint chunks or delta blobs on the startup path.
    let sql = if closed {
        "SELECT d.id,d.gen,d.meta,COALESCE(h.durable_rev,0),h.doc,COALESCE(v.view,'{}') FROM documents d LEFT JOIN recovery_heads h ON h.doc=d.id LEFT JOIN windows_views v ON v.doc=d.id WHERE d.state=1 ORDER BY d.rowid DESC LIMIT 1024"
    } else {
        "SELECT d.id,d.gen,d.meta,COALESCE(h.durable_rev,0),h.doc,COALESCE(v.view,'{}') FROM documents d LEFT JOIN windows_views v ON v.doc=d.id LEFT JOIN recovery_heads h ON h.doc=d.id WHERE d.state=0 AND (v.doc IS NOT NULL OR h.doc IS NOT NULL) ORDER BY v.position IS NULL,v.position,d.rowid LIMIT 1024"
    };
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut tabs = Vec::new();
    let mut warnings = Vec::new();
    while let Some(row) = rows.next()? {
        let id = match row
            .get::<_, String>(0)
            .map_err(ServiceError::from)
            .and_then(|id| parse_id(&id))
        {
            Ok(id) => id,
            Err(error) => {
                warnings.push(crate::RecoveryWarning { doc_id: None, message: format!("A recovery record has an invalid document id and was left untouched: {error}") });
                continue;
            }
        };
        let parsed = (|| -> Result<SessionTabStub, ServiceError> {
            let generation: i64 = row.get(1)?;
            let revision: i64 = row.get(3)?;
            if generation < 0 || revision < 0 {
                return Err(ServiceError::new(
                    ErrorKind::Corrupt,
                    "negative recovery generation/revision",
                ));
            }
            let meta = serde_json::from_str(&row.get::<_, String>(2)?)?;
            let view = match row
                .get::<_, String>(5)
                .map_err(ServiceError::from)
                .and_then(|json| {
                    if json == "{}" {
                        Ok(ViewState::default())
                    } else {
                        serde_json::from_str(&json).map_err(ServiceError::from)
                    }
                }) {
                Ok(view) => view,
                Err(error) => {
                    warnings.push(crate::RecoveryWarning {
                        doc_id: Some(id),
                        message: format!(
                            "Saved view position is damaged; reset only this tab's view: {error}"
                        ),
                    });
                    ViewState::default()
                }
            };
            Ok(SessionTabStub {
                doc_id: id,
                lifecycle_gen: LifecycleGen(generation as u64),
                durable_rev: Revision(revision as u64),
                meta,
                view,
                has_checkpoint: row.get::<_, Option<String>>(4)?.is_some(),
            })
        })();
        match parsed {
            Ok(tab) => tabs.push(tab),
            Err(error) => warnings.push(crate::RecoveryWarning { doc_id: Some(id), message: format!("Draft metadata is damaged; its stored content was preserved but this tab cannot be opened automatically: {error}") }),
        }
    }
    let active: Option<String> = conn
        .query_row("SELECT value FROM session WHERE key='active'", [], |r| {
            r.get(0)
        })
        .optional()?;
    let active_doc = match active.as_deref().map(parse_id).transpose() {
        Ok(active) => active.filter(|id| tabs.iter().any(|tab| tab.doc_id == *id)),
        Err(error) => {
            warnings.push(crate::RecoveryWarning {
                doc_id: None,
                message: format!("Active-tab metadata is damaged: {error}"),
            });
            None
        }
    };
    Ok(SessionManifest {
        tabs,
        active_doc,
        warnings,
    })
}
fn parse_id(value: &str) -> Result<DocumentId, ServiceError> {
    u128::from_str_radix(value, 16)
        .map(DocumentId)
        .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))
}
fn hydrate(conn: &Connection, doc_id: DocumentId) -> Result<HydratedDraft, ServiceError> {
    let id = doc_id.key();
    let (generation, state, meta_json): (i64, i64, String) = conn.query_row(
        "SELECT gen,state,meta FROM documents WHERE id=?1",
        [&id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if state == 2 {
        return Err(ServiceError::new(ErrorKind::Stale, "draft was discarded"));
    }
    let meta: DocMeta = serde_json::from_str(&meta_json)?;
    let (checkpoint_rev, durable_rev, expected_len, expected_hash): (i64, i64, i64, Vec<u8>) = conn.query_row("SELECT checkpoint_rev,durable_rev,byte_len,checkpoint_hash FROM recovery_heads WHERE doc=?1", [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    if expected_len < 0 || expected_len as usize > DOC_LIMIT {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "invalid draft length",
        ));
    }
    let mut builder = RopeBuilder::new();
    let mut hasher = blake3::Hasher::new();
    let mut loaded = 0usize;
    let mut statement = conn.prepare("SELECT ordinal,content,checksum,length(content) FROM checkpoint_chunks WHERE doc=?1 AND rev=?2 ORDER BY ordinal")?;
    let mut rows = statement.query(params![id, checkpoint_rev])?;
    let mut ordinal = 0i64;
    while let Some(row) = rows.next()? {
        let index: i64 = row.get(0)?;
        let byte_len: i64 = row.get(3)?;
        if byte_len < 0 || byte_len as usize > CHUNK_BYTES {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "oversized checkpoint chunk",
            ));
        }
        let bytes: Vec<u8> = row.get(1)?;
        let checksum: Vec<u8> = row.get(2)?;
        loaded += bytes.len();
        if index != ordinal
            || bytes.len() > CHUNK_BYTES
            || loaded > DOC_LIMIT
            || blake3::hash(&bytes).as_bytes().as_slice() != checksum
        {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "checkpoint chunk checksum/sequence failure",
            ));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))?;
        hasher.update(&bytes);
        builder.append(text);
        ordinal += 1;
    }
    if hasher.finalize().as_bytes().as_slice() != expected_hash {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "checkpoint checksum failure",
        ));
    }
    let mut rope = builder.finish();
    let mut rev = checkpoint_rev;
    let mut statement = conn.prepare("SELECT base_rev,new_rev,deltas,checksum,length(deltas) FROM edit_transactions WHERE doc=?1 AND new_rev>?2 ORDER BY new_rev")?;
    let mut rows = statement.query(params![id, checkpoint_rev])?;
    while let Some(row) = rows.next()? {
        let base: i64 = row.get(0)?;
        let next: i64 = row.get(1)?;
        let byte_len: i64 = row.get(4)?;
        if byte_len < 0 || byte_len as usize > PROMOTE_BYTES {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "oversized edit transaction",
            ));
        }
        let bytes: Vec<u8> = row.get(2)?;
        let checksum: Vec<u8> = row.get(3)?;
        if base != rev
            || next <= base
            || next > durable_rev
            || bytes.len() > PROMOTE_BYTES
            || blake3::hash(&bytes).as_bytes().as_slice() != checksum
        {
            return Err(ServiceError::new(
                ErrorKind::Corrupt,
                "edit log checksum or revision chain failure",
            ));
        }
        let deltas: Vec<TextDelta> = serde_json::from_slice(&bytes)?;
        rope = apply_deltas(rope, &deltas)?;
        rev = next;
    }
    if rev != durable_rev || rope.len() as i64 != expected_len {
        return Err(ServiceError::new(
            ErrorKind::Corrupt,
            "recovery head is not fully covered",
        ));
    }
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
fn layout(
    conn: &mut Connection,
    tabs: Vec<TabLayoutRecord>,
    active: Option<DocumentId>,
    meta_seq: u64,
    mailbox: &Mutex<Mailbox>,
) -> Result<(), ServiceError> {
    let result: Result<Vec<(DocumentId, LifecycleGen)>, ServiceError> = (|| {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut applied = Vec::with_capacity(tabs.len());
        tx.execute("DELETE FROM windows_views", [])?;
        for (position, tab) in tabs.iter().enumerate() {
            let id = tab.doc_id.key();
            let gen_ = sql_rev(tab.lifecycle_gen.0)?;
            let preserve_meta = mailbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .has_newer_meta(tab.doc_id, tab.lifecycle_gen, meta_seq);
            let meta = meta_for_source(&tx, &id, &tab.meta, preserve_meta)?;
            let changed = tx.execute("INSERT INTO documents(id,gen,state,meta) VALUES(?1,?2,0,?3) ON CONFLICT(id) DO UPDATE SET state=0,meta=excluded.meta WHERE documents.gen=excluded.gen AND documents.state<>2", params![id,gen_,serde_json::to_string(&meta)?])?;
            if changed != 0 {
                applied.push((tab.doc_id, tab.lifecycle_gen));
            }
            tx.execute("INSERT INTO windows_views(doc,position,view) SELECT id,?2,?3 FROM documents WHERE id=?1 AND gen=?4 AND state=0", params![id,position as i64,serde_json::to_string(&tab.view)?,gen_])?;
        }
        tx.execute("DELETE FROM session WHERE key='active'", [])?;
        if let Some(active) = active {
            tx.execute(
                "INSERT INTO session(key,value) VALUES('active',?1)",
                [active.key()],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    })();
    let mut box_ = mailbox.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(applied) = &result {
        for &(doc, gen_) in applied {
            box_.record_meta_applied(doc, gen_, meta_seq);
        }
    }
    for tab in &tabs {
        box_.release_meta(tab.doc_id);
    }
    result.map(|_| ())
}
fn close(
    conn: &mut Connection,
    doc: DocumentId,
    gen_: LifecycleGen,
    discard: bool,
    faults: &FaultInjector,
) -> Result<(), ServiceError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let id = doc.key();
    let generation = sql_rev(gen_.0)?;
    let old: Option<i64> = tx
        .query_row("SELECT gen FROM documents WHERE id=?1", [&id], |r| r.get(0))
        .optional()?;
    if old.is_some_and(|old| generation < old || (discard && generation <= old)) {
        return Err(ServiceError::new(
            ErrorKind::Stale,
            "discard requires a newer lifecycle",
        ));
    }
    tx.execute("INSERT INTO documents(id,gen,state,meta) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET gen=excluded.gen,state=excluded.state", params![id,generation,if discard {2} else {1},serde_json::to_string(&DocMeta::default())?])?;
    tx.execute("DELETE FROM windows_views WHERE doc=?1", [&id])?;
    if discard {
        tx.execute("DELETE FROM recovery_heads WHERE doc=?1", [&id])?;
        tx.execute("DELETE FROM checkpoint_chunks WHERE doc=?1", [&id])?;
        tx.execute("DELETE FROM edit_transactions WHERE doc=?1", [&id])?;
    }
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    Ok(())
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
mod metadata_order_tests {
    use super::*;
    use tempfile::TempDir;

    // Normal admission and real SQLite transactions, with worker steps scheduled
    // explicitly so both race orders are exercised without sleeps or intermediate flushes.
    struct Fixture {
        _dir: TempDir,
        conn: Connection,
        handle: RecoveryHandle,
        commands: Receiver<Command>,
        events: Sender<RecoveryEvent>,
        cache: SnapshotCache,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let conn = open_database(&dir.path().join("db")).unwrap();
            let (commands, receiver) = bounded(32);
            let (events, event_receiver) = bounded(256);
            let handle = RecoveryHandle {
                inner: Arc::new(HandleInner {
                    commands,
                    events: event_receiver,
                    mailbox: Arc::new(Mutex::new(Mailbox {
                        pending: HashMap::new(),
                        retained_bytes: 0,
                        inflight_bytes: 0,
                        accepting: true,
                        next_meta_seq: 0,
                        meta_state: HashMap::new(),
                    })),
                    last_error: Arc::new(Mutex::new(None)),
                    faults: FaultInjector::default(),
                }),
            };
            Self {
                _dir: dir,
                conn,
                handle,
                commands: receiver,
                events,
                cache: SnapshotCache::default(),
            }
        }

        fn drain(&mut self) -> Result<(), ServiceError> {
            drain(
                &mut self.conn,
                &self.handle.inner.mailbox,
                &self.events,
                &self.handle.inner.last_error,
                &self.handle.inner.faults,
                &mut self.cache,
            )
        }

        fn apply_next_layout(&mut self) -> Result<(), ServiceError> {
            loop {
                match self.commands.try_recv().expect("admitted layout command") {
                    Command::Wake => {}
                    Command::Layout {
                        tabs,
                        active,
                        meta_seq,
                    } => {
                        return layout(
                            &mut self.conn,
                            tabs,
                            active,
                            meta_seq,
                            &self.handle.inner.mailbox,
                        );
                    }
                    _ => panic!("unexpected worker command"),
                }
            }
        }

        fn assert_idle(&self) {
            let mailbox = self.handle.inner.mailbox.lock().unwrap();
            assert!(mailbox.meta_state.is_empty());
            assert!(mailbox.pending.is_empty());
            assert_eq!(mailbox.retained_bytes, 0);
            assert_eq!(mailbox.inflight_bytes, 0);
        }
    }

    fn current(label: &str, encoding: EncodingSpec, line_ending: LineEnding) -> DocMeta {
        DocMeta {
            title: label.into(),
            encoding,
            line_ending,
            language_override: Some(label.into()),
            ..DocMeta::default()
        }
    }
    fn commit(id: DocumentId, revision: u64, meta: DocMeta) -> RecoveryCommit {
        RecoveryCommit {
            doc_id: id,
            lifecycle_gen: LifecycleGen(0),
            base_rev: Revision(revision.saturating_sub(1)),
            new_rev: Revision(revision),
            rope: Rope::from_str("committed text\n"),
            deltas: Vec::new(),
            meta,
        }
    }
    fn tab(id: DocumentId, meta: DocMeta) -> TabLayoutRecord {
        TabLayoutRecord {
            doc_id: id,
            lifecycle_gen: LifecycleGen(0),
            meta,
            view: ViewState {
                cursor_byte: 3,
                scroll_byte: 1,
            },
        }
    }
    fn assert_current(actual: &DocMeta, expected: &DocMeta) {
        assert_eq!(actual.title, expected.title);
        assert_eq!(actual.encoding, expected.encoding);
        assert_eq!(actual.line_ending, expected.line_ending);
        assert_eq!(actual.language_override, expected.language_override);
    }

    #[test]
    fn newest_admitted_metadata_wins_in_both_worker_orders_with_saved_baseline() {
        for commit_first in [false, true] {
            for layout_first in [false, true] {
                let mut fixture = Fixture::new();
                let id = DocumentId::new();
                let identity = ContentIdentity::of_rope(&Rope::from_str("previous save"));
                let intent = SaveIntent {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(0),
                    captured_rev: Revision(0),
                    target_path: fixture._dir.path().join("saved.txt"),
                    captured_identity: identity,
                    captured_encoding: Some(EncodingSpec::Utf8Bom),
                    captured_line_ending: Some(LineEnding::Lf),
                    stage: SaveStage::Published,
                    output_fingerprint: None,
                };
                record_intent(&mut fixture.conn, &intent, &fixture.handle.inner.faults).unwrap();
                let commit_meta = current("rust", EncodingSpec::Utf8Bom, LineEnding::Crlf);
                let layout_meta = current("python", EncodingSpec::Utf8, LineEnding::Lf);
                let job = commit(id, 1, commit_meta.clone());
                let tabs = vec![tab(id, layout_meta.clone())];
                if commit_first {
                    fixture.handle.try_commit(job).unwrap();
                    fixture.handle.save_session_layout(tabs, Some(id)).unwrap();
                } else {
                    fixture.handle.save_session_layout(tabs, Some(id)).unwrap();
                    fixture.handle.try_commit(job).unwrap();
                }
                if layout_first {
                    fixture.apply_next_layout().unwrap();
                    fixture.drain().unwrap();
                } else {
                    fixture.drain().unwrap();
                    fixture.apply_next_layout().unwrap();
                }
                let expected = if commit_first {
                    &layout_meta
                } else {
                    &commit_meta
                };
                let draft = hydrate(&fixture.conn, id).unwrap();
                assert_current(&draft.meta, expected);
                assert_eq!(draft.meta.saved_identity, Some(identity));
                assert_eq!(draft.meta.saved_encoding, Some(EncodingSpec::Utf8Bom));
                assert_eq!(draft.meta.saved_line_ending, Some(LineEnding::Lf));
                assert_eq!(draft.rope, "committed text\n");
                assert_eq!(draft.durable_rev, Revision(1));
                let manifest = manifest(&fixture.conn, false).unwrap();
                assert_current(&manifest.tabs[0].meta, expected);
                assert_eq!(manifest.tabs[0].view.cursor_byte, 3);
                fixture.assert_idle();
            }
        }
    }

    #[test]
    fn applied_stamp_never_regresses_across_three_out_of_order_sources() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        fixture
            .handle
            .save_session_layout(
                vec![tab(id, current("old", EncodingSpec::Utf8, LineEnding::Lf))],
                Some(id),
            )
            .unwrap();
        fixture
            .handle
            .save_session_layout(
                vec![tab(
                    id,
                    current("middle", EncodingSpec::Utf8, LineEnding::Crlf),
                )],
                Some(id),
            )
            .unwrap();
        let newest = current("newest", EncodingSpec::Utf8Bom, LineEnding::Crlf);
        fixture
            .handle
            .try_commit(commit(id, 1, newest.clone()))
            .unwrap();
        fixture.drain().unwrap();
        let applied = fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].applied;
        fixture.apply_next_layout().unwrap();
        assert_eq!(
            fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].applied,
            applied
        );
        fixture.apply_next_layout().unwrap();
        assert_current(&hydrate(&fixture.conn, id).unwrap().meta, &newest);
        fixture.assert_idle();
    }

    #[test]
    fn retry_keeps_original_stamp_and_newer_layout_survives() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        fixture
            .handle
            .try_commit(commit(id, 1, DocMeta::default()))
            .unwrap();
        let seq = fixture.handle.inner.mailbox.lock().unwrap().pending[&id].meta_seq;
        fixture
            .handle
            .inner
            .faults
            .arm(FaultPoint::RecoveryBeforeCommit);
        assert_eq!(fixture.drain().unwrap_err().kind, ErrorKind::Injected);
        {
            let mailbox = fixture.handle.inner.mailbox.lock().unwrap();
            assert_eq!(mailbox.pending[&id].meta_seq, seq);
            assert_eq!(mailbox.meta_state[&id].outstanding, 1);
            assert!(mailbox.meta_state[&id].applied.is_none());
        }
        let newest = current("python", EncodingSpec::Utf8Bom, LineEnding::Crlf);
        fixture
            .handle
            .save_session_layout(vec![tab(id, newest.clone())], Some(id))
            .unwrap();
        fixture.apply_next_layout().unwrap();
        fixture.drain().unwrap();
        assert_current(&hydrate(&fixture.conn, id).unwrap().meta, &newest);
        fixture.assert_idle();
        let after_idle = current("rust", EncodingSpec::Utf8, LineEnding::Lf);
        fixture
            .handle
            .try_commit(commit(id, 2, after_idle.clone()))
            .unwrap();
        fixture.drain().unwrap();
        assert_current(&hydrate(&fixture.conn, id).unwrap().meta, &after_idle);
        fixture.assert_idle();
    }

    #[test]
    fn superseded_inflight_failure_releases_only_its_own_source() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        fixture
            .handle
            .try_commit(commit(id, 1, DocMeta::default()))
            .unwrap();
        let (pending, preserve_meta) = take_pending(&fixture.handle.inner.mailbox, id).unwrap();
        let newest = current("python", EncodingSpec::Utf8Bom, LineEnding::Crlf);
        fixture
            .handle
            .try_commit(commit(id, 2, newest.clone()))
            .unwrap();
        assert_eq!(
            fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
            2
        );
        fixture
            .handle
            .inner
            .faults
            .arm(FaultPoint::RecoveryBeforeCommit);
        let result = commit_one(
            &mut fixture.conn,
            &pending.commit,
            preserve_meta,
            &fixture.handle.inner.faults,
            &mut fixture.cache,
        );
        assert_eq!(result.as_ref().unwrap_err().kind, ErrorKind::Injected);
        finish_commit(&fixture.handle.inner.mailbox, pending, &result);
        assert_eq!(
            fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
            1
        );
        fixture.drain().unwrap();
        let draft = hydrate(&fixture.conn, id).unwrap();
        assert_current(&draft.meta, &newest);
        assert_eq!(draft.durable_rev, Revision(2));
        fixture.assert_idle();
    }

    #[test]
    fn failed_layout_transaction_does_not_advance_metadata_stamp() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        let old = current("old", EncodingSpec::Utf8, LineEnding::Lf);
        fixture
            .handle
            .try_commit(commit(id, 1, old.clone()))
            .unwrap();
        fixture
            .handle
            .save_session_layout(
                vec![tab(
                    id,
                    current("failed", EncodingSpec::Utf8Bom, LineEnding::Crlf),
                )],
                Some(id),
            )
            .unwrap();
        fixture
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER reject_layout BEFORE INSERT ON session
             BEGIN SELECT RAISE(ABORT, 'layout failure'); END;",
            )
            .unwrap();
        assert_eq!(
            fixture.apply_next_layout().unwrap_err().kind,
            ErrorKind::Storage
        );
        {
            let mailbox = fixture.handle.inner.mailbox.lock().unwrap();
            assert_eq!(mailbox.meta_state[&id].outstanding, 1);
            assert!(mailbox.meta_state[&id].applied.is_none());
        }
        fixture
            .conn
            .execute_batch("DROP TRIGGER reject_layout")
            .unwrap();
        fixture.drain().unwrap();
        assert_current(&hydrate(&fixture.conn, id).unwrap().meta, &old);
        fixture.assert_idle();
    }

    #[test]
    fn metadata_refs_balance_on_coalescing_admission_failure_and_terminal_errors() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        for _ in 0..32 {
            fixture
                .handle
                .inner
                .commands
                .try_send(Command::Wake)
                .unwrap();
        }
        let duplicate = tab(id, DocMeta::default());
        assert_eq!(
            fixture
                .handle
                .save_session_layout(vec![duplicate.clone(), duplicate], Some(id))
                .unwrap_err()
                .kind,
            ErrorKind::Busy,
        );
        fixture.assert_idle();
        assert_eq!(fixture.commands.try_iter().count(), 32);
        fixture
            .handle
            .try_commit(commit(id, 1, DocMeta::default()))
            .unwrap();
        fixture
            .handle
            .try_commit(commit(id, 2, DocMeta::default()))
            .unwrap();
        assert_eq!(
            fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
            1
        );
        fixture.drain().unwrap();
        fixture.assert_idle();
        // Invalid SQLite revision is terminal, not a retained/re-stamped retry.
        fixture
            .handle
            .try_commit(commit(id, u64::MAX, DocMeta::default()))
            .unwrap();
        assert_eq!(fixture.drain().unwrap_err().kind, ErrorKind::InvalidInput);
        fixture.assert_idle();
    }

    #[test]
    fn close_does_not_release_queued_sources_or_stamp_rejected_layouts() {
        let mut fixture = Fixture::new();
        let id = DocumentId::new();
        fixture
            .handle
            .try_commit(commit(id, 1, DocMeta::default()))
            .unwrap();
        fixture
            .handle
            .save_session_layout(vec![tab(id, DocMeta::default())], Some(id))
            .unwrap();
        close(
            &mut fixture.conn,
            id,
            LifecycleGen(1),
            true,
            &fixture.handle.inner.faults,
        )
        .unwrap();
        assert_eq!(
            fixture.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
            2
        );
        fixture.apply_next_layout().unwrap();
        {
            let mailbox = fixture.handle.inner.mailbox.lock().unwrap();
            assert_eq!(mailbox.meta_state[&id].outstanding, 1);
            assert!(mailbox.meta_state[&id].applied.is_none());
        }
        fixture.drain().unwrap();
        fixture.assert_idle();
    }

    #[test]
    fn missing_row_save_acknowledgement_uses_captured_current_format() {
        let mut fixture = Fixture::new();
        for captured in [false, true] {
            let intent = SaveIntent {
                doc_id: DocumentId::new(),
                lifecycle_gen: LifecycleGen(0),
                captured_rev: Revision(1),
                target_path: fixture._dir.path().join("saved.txt"),
                captured_identity: ContentIdentity::of_rope(&Rope::from_str("saved")),
                captured_encoding: captured.then_some(EncodingSpec::Utf8Bom),
                captured_line_ending: captured.then_some(LineEnding::Crlf),
                stage: SaveStage::Published,
                output_fingerprint: None,
            };
            record_intent(&mut fixture.conn, &intent, &fixture.handle.inner.faults).unwrap();
            let json: String = fixture
                .conn
                .query_row(
                    "SELECT meta FROM documents WHERE id=?1",
                    [intent.doc_id.key()],
                    |row| row.get(0),
                )
                .unwrap();
            let meta: DocMeta = serde_json::from_str(&json).unwrap();
            assert_eq!(meta.encoding, intent.captured_encoding.unwrap_or_default());
            assert_eq!(
                meta.line_ending,
                intent.captured_line_ending.unwrap_or_default()
            );
            assert_eq!(meta.saved_encoding, intent.captured_encoding);
            assert_eq!(meta.saved_line_ending, intent.captured_line_ending);
        }
    }
}
