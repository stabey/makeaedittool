use crate::{
    CancelToken, DocumentId, EncodingSpec, ErrorKind, PagedDocHandle, QueryId, Revision,
    ServiceError, SourceGeneration,
};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use ropey::Rope;
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Condvar, LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchAlgorithm {
    MemchrBaseline,
    ByteScanExperiment,
}
#[derive(Clone, Debug)]
pub enum SearchTarget {
    EditableSnapshot {
        doc_id: DocumentId,
        rev: Revision,
        rope: Rope,
    },
    PagedFile {
        handle: PagedDocHandle,
        source_gen: SourceGeneration,
        encoding: EncodingSpec,
    },
}
impl SearchTarget {
    fn doc_id(&self) -> DocumentId {
        match self {
            Self::EditableSnapshot { doc_id, .. } => *doc_id,
            Self::PagedFile { handle, .. } => handle.doc_id(),
        }
    }
    fn len(&self) -> u64 {
        match self {
            Self::EditableSnapshot { rope, .. } => rope.len() as u64,
            Self::PagedFile { handle, .. } => handle.len(),
        }
    }
    fn read_at(&self, at: u64, buffer: &mut [u8]) -> Result<usize, ServiceError> {
        match self {
            Self::PagedFile {
                handle,
                source_gen,
                encoding,
            } => {
                if *encoding == EncodingSpec::UnsupportedReadOnly {
                    return Err(ServiceError::new(
                        ErrorKind::Unsupported,
                        "literal search requires UTF-8; unsupported encoding",
                    ));
                }
                handle.read_at(at, buffer, *source_gen)
            }
            Self::EditableSnapshot { rope, .. } => {
                let wanted = buffer.len().min(rope.len().saturating_sub(at as usize));
                let mut done = 0;
                while done < wanted {
                    let position = at as usize + done;
                    let (chunk, start) = rope.chunk(position);
                    let within = position - start;
                    let take = (wanted - done).min(chunk.len() - within);
                    buffer[done..done + take]
                        .copy_from_slice(&chunk.as_bytes()[within..within + take]);
                    done += take;
                }
                Ok(done)
            }
        }
    }
    fn validate(&self) -> Result<(), ServiceError> {
        match self {
            Self::PagedFile {
                handle, source_gen, ..
            } => handle.validate(*source_gen),
            _ => Ok(()),
        }
    }
}
#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query_id: QueryId,
    pub pattern: Vec<u8>,
    pub algorithm: SearchAlgorithm,
    pub target: SearchTarget,
    pub max_matches: usize,
    pub block_bytes: usize,
    pub cancel: CancelToken,
}
impl SearchRequest {
    pub fn new(query_id: QueryId, pattern: impl Into<Vec<u8>>, target: SearchTarget) -> Self {
        Self {
            query_id,
            pattern: pattern.into(),
            algorithm: SearchAlgorithm::MemchrBaseline,
            target,
            max_matches: 10_000,
            block_bytes: 1024 * 1024,
            cancel: CancelToken::new(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub byte_range: Range<u64>,
    pub line_number: Option<u64>,
    pub preview: Box<str>,
    pub preview_match_range: Range<usize>,
}
#[derive(Clone, Debug)]
pub enum SearchEvent {
    Batch {
        query_id: QueryId,
        matches: Vec<SearchMatch>,
    },
    Done {
        query_id: QueryId,
        total_emitted: usize,
        truncated: bool,
        count_exact: bool,
        elapsed_ns: u64,
        bytes_scanned: u64,
        bytes_read: u64,
    },
    Cancelled {
        query_id: QueryId,
    },
    Invalidated {
        query_id: QueryId,
        new_gen: SourceGeneration,
    },
    Failed {
        query_id: QueryId,
        error: ServiceError,
    },
}
#[derive(Debug)]
pub struct SearchTask {
    pub events: Receiver<SearchEvent>,
    pub cancel: CancelToken,
    _service: SearchService,
}
impl Drop for SearchTask {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
#[derive(Clone, Debug)]
pub struct SearchService {
    owner: Arc<SearchOwner>,
}
#[derive(Debug)]
struct SearchOwner {
    pool: Arc<SearchPool>,
}
#[derive(Debug)]
struct SearchPool {
    state: Mutex<SearchState>,
    wake: Condvar,
}
#[derive(Debug)]
struct SearchState {
    slots: HashMap<DocumentId, SearchSlot>,
    limit: usize,
    stopping: bool,
    startup_error: Option<ServiceError>,
}
#[derive(Debug)]
struct SearchSlot {
    running: Option<CancelToken>,
    pending: Option<SearchJob>,
}
#[derive(Debug)]
struct SearchJob {
    request: SearchRequest,
    sender: Sender<SearchEvent>,
}
impl Drop for SearchOwner {
    fn drop(&mut self) {
        let mut state = self.pool.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopping = true;
        for slot in state.slots.values() {
            if let Some(cancel) = &slot.running {
                cancel.cancel();
            }
            if let Some(job) = &slot.pending {
                job.request.cancel.cancel();
            }
        }
        drop(state);
        self.pool.wake.notify_all();
    }
}
impl Default for SearchService {
    fn default() -> Self {
        static GLOBAL: LazyLock<SearchService> = LazyLock::new(|| SearchService::with_limit(4));
        GLOBAL.clone()
    }
}
impl SearchService {
    pub fn with_limit(limit: usize) -> Self {
        let limit = limit.clamp(1, 4);
        let pool = Arc::new(SearchPool {
            state: Mutex::new(SearchState {
                slots: HashMap::new(),
                limit,
                stopping: false,
                startup_error: None,
            }),
            wake: Condvar::new(),
        });
        for index in 0..limit {
            let worker_pool = pool.clone();
            if let Err(error) = std::thread::Builder::new()
                .name(format!("literal-search-{index}"))
                .spawn(move || search_worker(worker_pool))
            {
                let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
                state.startup_error = Some(error.into());
                state.stopping = true;
                drop(state);
                pool.wake.notify_all();
                break;
            }
        }
        Self {
            owner: Arc::new(SearchOwner { pool }),
        }
    }
    /// Occupied document slots, including a running query and its latest pending replacement.
    pub fn active_tasks(&self) -> usize {
        self.owner
            .pool
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .len()
    }
    pub fn start(&self, request: SearchRequest) -> Result<SearchTask, ServiceError> {
        if request.pattern.len() > 65_536 || std::str::from_utf8(&request.pattern).is_err() {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "pattern must be UTF-8 and at most 64 KiB",
            ));
        }
        if request.block_bytes == 0 || request.block_bytes > 8 * 1024 * 1024 {
            return Err(ServiceError::new(
                ErrorKind::InvalidInput,
                "block size must be 1 byte through 8 MiB",
            ));
        }
        let (sender, events) = bounded(8);
        let cancel = request.cancel.clone();
        let doc_id = request.target.doc_id();
        // This mutex protects at most four metadata slots; no I/O or text scanning
        // runs while locked. Same-document supersession never waits for an old read.
        let mut state = self
            .owner
            .pool
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(error) = &state.startup_error {
            return Err(error.clone());
        }
        if state.stopping {
            return Err(ServiceError::new(
                ErrorKind::Cancelled,
                "search service is stopping",
            ));
        }
        if !state.slots.contains_key(&doc_id) && state.slots.len() >= state.limit {
            return Err(ServiceError::busy());
        }
        let slot = state.slots.entry(doc_id).or_insert(SearchSlot {
            running: None,
            pending: None,
        });
        if let Some(running) = &slot.running {
            running.cancel();
        }
        let obsolete = slot.pending.replace(SearchJob { request, sender });
        if let Some(obsolete) = &obsolete {
            obsolete.request.cancel.cancel();
        }
        drop(state);
        drop(obsolete);
        self.owner.pool.wake.notify_one();
        Ok(SearchTask {
            events,
            cancel,
            _service: self.clone(),
        })
    }
}
fn search_worker(pool: Arc<SearchPool>) {
    loop {
        let job = {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if state.stopping {
                    return;
                }
                let ready = state
                    .slots
                    .iter()
                    .find(|(_, slot)| slot.running.is_none() && slot.pending.is_some())
                    .map(|(id, _)| *id);
                if let Some(id) = ready {
                    if let Some(slot) = state.slots.get_mut(&id) {
                        if let Some(job) = slot.pending.take() {
                            slot.running = Some(job.request.cancel.clone());
                            break job;
                        }
                    }
                }
                state = pool.wake.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        let request = &job.request;
        if let Err(error) = scan(request, &job.sender) {
            let event = if error.kind == ErrorKind::Cancelled {
                SearchEvent::Cancelled {
                    query_id: request.query_id,
                }
            } else if error.kind == ErrorKind::SourceChanged {
                let new_gen = match &request.target {
                    SearchTarget::PagedFile { handle, .. } => handle.source_generation(),
                    _ => SourceGeneration(0),
                };
                SearchEvent::Invalidated {
                    query_id: request.query_id,
                    new_gen,
                }
            } else {
                SearchEvent::Failed {
                    query_id: request.query_id,
                    error,
                }
            };
            if matches!(event, SearchEvent::Cancelled { .. }) {
                let _ = job.sender.try_send(event);
            } else {
                let _ = send(&job.sender, event, &request.cancel);
            }
        }
        let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
        let id = request.target.doc_id();
        if let Some(slot) = state.slots.get_mut(&id) {
            slot.running = None;
            if slot.pending.is_none() {
                state.slots.remove(&id);
            }
        }
        drop(state);
        pool.wake.notify_all();
    }
}
fn send(
    tx: &Sender<SearchEvent>,
    mut event: SearchEvent,
    cancel: &CancelToken,
) -> Result<(), ServiceError> {
    loop {
        cancel.check()?;
        match tx.send_timeout(event, Duration::from_millis(10)) {
            Ok(()) => return Ok(()),
            Err(SendTimeoutError::Timeout(returned)) => event = returned,
            Err(SendTimeoutError::Disconnected(_)) => {
                return Err(ServiceError::new(
                    ErrorKind::Cancelled,
                    "result consumer closed",
                ));
            }
        }
    }
}
fn simple_find(
    haystack: &[u8],
    needle: &[u8],
    cancel: &CancelToken,
) -> Result<Option<usize>, ServiceError> {
    // Deliberately simple reference/experiment kernel; periodically check even the
    // adversarial repeated-prefix case rather than treating one scan as uninterruptible.
    for (index, window) in haystack.windows(needle.len()).enumerate() {
        if index % 1024 == 0 {
            cancel.check()?;
        }
        if window[0] == needle[0]
            && window[needle.len() - 1] == needle[needle.len() - 1]
            && window == needle
        {
            return Ok(Some(index));
        }
    }
    Ok(None)
}
fn preview(bytes: &[u8], start: usize, end: usize, base: u64) -> SearchMatch {
    let mut lo = start.saturating_sub(64);
    let mut hi = (lo + 256).min(bytes.len());
    while lo < start && bytes[lo] & 0xc0 == 0x80 {
        lo += 1;
    }
    while hi > lo && hi < bytes.len() && bytes[hi] & 0xc0 == 0x80 {
        hi -= 1;
    }
    let value = String::from_utf8_lossy(&bytes[lo..hi]);
    let before = String::from_utf8_lossy(&bytes[lo..start.min(hi)]).len();
    let matched = String::from_utf8_lossy(&bytes[start.min(hi)..end.min(hi)]).len();
    let mut capped = value.len().min(256);
    while !value.is_char_boundary(capped) {
        capped -= 1;
    }
    SearchMatch {
        byte_range: (base + start as u64)..(base + end as u64),
        line_number: None,
        preview: value[..capped].into(),
        preview_match_range: before.min(capped)..(before + matched).min(capped),
    }
}
fn reject_unsupported_bom(prefix: &[u8]) -> Result<(), ServiceError> {
    if prefix.starts_with(&[0xff, 0xfe])
        || prefix.starts_with(&[0xfe, 0xff])
        || prefix.starts_with(&[0, 0, 0xfe, 0xff])
    {
        Err(ServiceError::new(
            ErrorKind::Unsupported,
            "UTF-16/32 literal search is not supported",
        ))
    } else {
        Ok(())
    }
}
fn scan(request: &SearchRequest, tx: &Sender<SearchEvent>) -> Result<(), ServiceError> {
    let started = Instant::now();
    let pattern = &request.pattern;
    let max_matches = request.max_matches.min(10_000);
    if pattern.is_empty() || max_matches == 0 {
        return send(
            tx,
            SearchEvent::Done {
                query_id: request.query_id,
                total_emitted: 0,
                truncated: max_matches == 0 && !pattern.is_empty(),
                count_exact: pattern.is_empty(),
                elapsed_ns: 0,
                bytes_scanned: 0,
                bytes_read: 0,
            },
            &request.cancel,
        );
    }
    let mut probe_bytes = 0;
    if request.block_bytes < 4 && matches!(request.target, SearchTarget::PagedFile { .. }) {
        // Tiny correctness-test blocks cannot contain a full BOM. Normal I/O
        // blocks inspect their first bytes directly without an extra read/stat.
        let mut prefix = [0u8; 4];
        probe_bytes = request.target.read_at(0, &mut prefix)? as u64;
        reject_unsupported_bom(&prefix[..probe_bytes as usize])?;
    }
    let finder = memchr::memmem::Finder::new(pattern);
    let mut buffer = vec![0u8; request.block_bytes + pattern.len() - 1];
    let mut carry = 0;
    let mut read_offset = 0u64;
    let mut next_allowed = 0u64;
    let mut total = 0;
    let mut batch = Vec::with_capacity(128);
    let mut last_batch = Instant::now();
    let mut truncated = false;
    while read_offset < request.target.len() {
        request.cancel.check()?;
        let n = request
            .target
            .read_at(read_offset, &mut buffer[carry..carry + request.block_bytes])?;
        if n == 0 {
            break;
        }
        if read_offset == 0 && matches!(request.target, SearchTarget::PagedFile { .. }) {
            reject_unsupported_bom(&buffer[carry..carry + n.min(4)])?;
        }
        let base = read_offset - carry as u64;
        read_offset += n as u64;
        let valid = carry + n;
        let mut cursor = next_allowed.saturating_sub(base).min(valid as u64) as usize;
        while cursor + pattern.len() <= valid {
            request.cancel.check()?;
            let found = match request.algorithm {
                SearchAlgorithm::MemchrBaseline => finder.find(&buffer[cursor..valid]),
                SearchAlgorithm::ByteScanExperiment => {
                    simple_find(&buffer[cursor..valid], pattern, &request.cancel)?
                }
            };
            let Some(relative) = found else {
                break;
            };
            let start = cursor + relative;
            let end = start + pattern.len();
            batch.push(preview(&buffer[..valid], start, end, base));
            next_allowed = base + end as u64;
            cursor = end;
            total += 1;
            if batch.len() == 128 || last_batch.elapsed() >= Duration::from_millis(50) {
                send(
                    tx,
                    SearchEvent::Batch {
                        query_id: request.query_id,
                        matches: std::mem::replace(&mut batch, Vec::with_capacity(128)),
                    },
                    &request.cancel,
                )?;
                last_batch = Instant::now();
            }
            if total == max_matches {
                truncated = true;
                break;
            }
        }
        if truncated {
            break;
        }
        let keep_from = read_offset
            .saturating_sub((pattern.len() - 1) as u64)
            .max(next_allowed)
            .max(base);
        carry = (read_offset - keep_from) as usize;
        buffer.copy_within(valid - carry..valid, 0);
        if !batch.is_empty() && last_batch.elapsed() >= Duration::from_millis(50) {
            send(
                tx,
                SearchEvent::Batch {
                    query_id: request.query_id,
                    matches: std::mem::replace(&mut batch, Vec::with_capacity(128)),
                },
                &request.cancel,
            )?;
            last_batch = Instant::now();
        }
    }
    request.target.validate()?;
    if !batch.is_empty() {
        send(
            tx,
            SearchEvent::Batch {
                query_id: request.query_id,
                matches: batch,
            },
            &request.cancel,
        )?;
    }
    send(
        tx,
        SearchEvent::Done {
            query_id: request.query_id,
            total_emitted: total,
            truncated,
            count_exact: !truncated,
            elapsed_ns: started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            bytes_scanned: if truncated { next_allowed } else { read_offset },
            bytes_read: read_offset + probe_bytes,
        },
        &request.cancel,
    )
}
