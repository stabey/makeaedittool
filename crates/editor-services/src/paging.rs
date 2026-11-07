use crate::{CancelToken, DocumentId, ErrorKind, FileFingerprint, ServiceError, SourceGeneration};
use std::{
    collections::HashMap,
    fs::File,
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

pub const VIEWPORT_BYTES: usize = 64 * 1024;
const PAGE_BYTES: usize = 64 * 1024;
const MIN_INDEX_STRIDE: u64 = 64 * 1024;
static NEXT_SOURCE: AtomicU64 = AtomicU64::new(1);
const INDEX_BLOCK_BYTES: usize = 1024 * 1024;
static INDEX_JOBS: IoBudget = IoBudget::new(2);
static OPEN_JOBS: IoBudget = IoBudget::new(2);
static INDEX_BUFFERS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

struct IoBudget {
    active: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
}
impl IoBudget {
    const fn new(limit: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit,
        }
    }
    fn acquire(&'static self) -> Result<IoPermit, ServiceError> {
        let previous = self
            .active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                if active < self.limit {
                    Some(active + 1)
                } else {
                    None
                }
            })
            .map_err(|_| ServiceError::busy())?;
        self.peak.fetch_max(previous + 1, Ordering::Relaxed);
        Ok(IoPermit(self))
    }
}
pub(crate) struct IoPermit(&'static IoBudget);
impl Drop for IoPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Release);
    }
}
pub(crate) fn acquire_open_permit() -> Result<IoPermit, ServiceError> {
    OPEN_JOBS.acquire()
}
struct IndexBuffer {
    bytes: Vec<u8>,
    _permit: IoPermit,
}
impl IndexBuffer {
    fn acquire() -> Result<Self, ServiceError> {
        let permit = INDEX_JOBS.acquire()?;
        let bytes = INDEX_BUFFERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap_or_else(|| vec![0; INDEX_BLOCK_BYTES]);
        Ok(Self {
            bytes,
            _permit: permit,
        })
    }
}
impl Drop for IndexBuffer {
    fn drop(&mut self) {
        INDEX_BUFFERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(std::mem::take(&mut self.bytes));
    }
}

#[derive(Clone, Debug)]
pub enum ViewportAnchor {
    ByteOffset(u64),
    ApproxLine(u64),
}
#[derive(Clone, Debug)]
pub struct ViewportRequest {
    pub source_gen: SourceGeneration,
    pub anchor: ViewportAnchor,
    pub max_bytes: usize,
}
#[derive(Clone, Debug)]
pub struct ViewportSlice {
    /// With `lossy == true`, decoded text offsets do not map one-to-one to raw bytes.
    /// `byte_range` covers exactly the consumed raw prefix; internal UI row offsets
    /// must be shown as unknown rather than added to its start.
    pub source_gen: SourceGeneration,
    pub byte_range: Range<u64>,
    pub text: Arc<str>,
    pub first_line_number: Option<u64>,
    pub is_line_fragment: bool,
    pub lossy: bool,
    pub index_status: IndexStatus,
}
#[derive(Clone, Copy, Debug)]
pub enum IndexStatus {
    Indexing {
        indexed_bytes: u64,
        total_bytes: u64,
        known_lines: u64,
    },
    Complete {
        total_bytes: u64,
        total_lines: u64,
    },
}
#[derive(Clone, Copy, Debug, Default)]
pub struct CacheStats {
    pub cached_bytes: usize,
    pub cache_budget: usize,
    pub index_bytes: usize,
    pub index_budget: usize,
    pub pages: usize,
    pub sources: usize,
    pub active_index_jobs: usize,
    pub peak_index_jobs: usize,
    pub active_open_jobs: usize,
    pub peak_open_jobs: usize,
}
#[derive(Clone, Debug)]
pub struct PagingService {
    inner: Arc<PagingInner>,
}
#[derive(Debug)]
struct PagingInner {
    cache: Mutex<Cache>,
    indices: Mutex<Indices>,
}
#[derive(Debug)]
struct Cache {
    pages: HashMap<(u64, u64), Page>,
    bytes: usize,
    budget: usize,
    clock: u64,
}
#[derive(Debug)]
struct Page {
    data: Arc<Vec<u8>>,
    touched: u64,
}
#[derive(Clone, Copy, Debug)]
struct Point {
    byte: u64,
    line: u64,
}
#[derive(Debug)]
struct SparseIndex {
    points: Vec<Point>,
    stride: u64,
    indexed: u64,
    newlines: u64,
    total: u64,
}
#[derive(Debug)]
struct Indices {
    docs: HashMap<u64, SparseIndex>,
    budget: usize,
}
impl Indices {
    fn push_point(&mut self, id: u64, point: Point) {
        loop {
            let Some(index) = self.docs.get(&id) else {
                return;
            };
            if point.byte % index.stride != 0 {
                return;
            }
            if index.points.len() < index.points.capacity() {
                if let Some(index) = self.docs.get_mut(&id) {
                    index.points.push(point);
                }
                return;
            }
            let allocated: usize = self
                .docs
                .values()
                .map(|index| index.points.capacity())
                .sum();
            let free = (self.budget / std::mem::size_of::<Point>()).saturating_sub(allocated);
            if free > 0 {
                if let Some(index) = self.docs.get_mut(&id) {
                    let increment = index.points.capacity().max(16).min(free);
                    index.points.reserve_exact(increment);
                }
                continue;
            }
            // Reclaim unused allocation first, otherwise thin the largest index.
            // No document keeps a fixed reservation while other files starve.
            let victim = self
                .docs
                .iter()
                .filter(|(_, index)| {
                    index.points.capacity() > index.points.len() || index.points.len() > 1
                })
                .max_by_key(|(_, index)| index.points.capacity())
                .map(|(id, _)| *id);
            let Some(victim) = victim else {
                return;
            };
            if let Some(index) = self.docs.get_mut(&victim) {
                let before = index.points.capacity();
                if index.points.len() == before {
                    index.stride = index.stride.saturating_mul(2);
                    let stride = index.stride;
                    index.points.retain(|point| point.byte % stride == 0);
                }
                index.points.shrink_to_fit();
                if index.points.capacity() >= before {
                    return;
                }
            }
        }
    }
}
#[derive(Clone, Debug)]
pub struct PagedDocHandle {
    inner: Arc<PagedFile>,
}
#[derive(Debug)]
struct PagedFile {
    id: u64,
    pub doc_id: DocumentId,
    path: PathBuf,
    file: File,
    fingerprint: FileFingerprint,
    open_gen: u64,
    generation: AtomicU64,
    has_cached_pages: AtomicBool,
    service: PagingService,
}
impl Drop for PagedFile {
    fn drop(&mut self) {
        self.service
            .invalidate(self.id, self.has_cached_pages.load(Ordering::Relaxed));
    }
}
impl Default for PagingService {
    fn default() -> Self {
        static GLOBAL: LazyLock<PagingService> =
            LazyLock::new(|| PagingService::with_budgets(64 * 1024 * 1024, 16 * 1024 * 1024));
        GLOBAL.clone()
    }
}
impl PagingService {
    /// Construct one budget domain. Applications should use the shared `default()`.
    pub fn with_budgets(cache_bytes: usize, index_bytes: usize) -> Self {
        Self {
            inner: Arc::new(PagingInner {
                cache: Mutex::new(Cache {
                    pages: HashMap::new(),
                    bytes: 0,
                    budget: cache_bytes.max(PAGE_BYTES),
                    clock: 0,
                }),
                indices: Mutex::new(Indices {
                    docs: HashMap::new(),
                    budget: index_bytes,
                }),
            }),
        }
    }
    pub fn open(
        &self,
        doc_id: DocumentId,
        path: impl AsRef<Path>,
    ) -> Result<PagedDocHandle, ServiceError> {
        let path = std::path::absolute(path)?;
        let file = File::open(&path)?;
        let fingerprint = FileFingerprint::of_metadata(&file.metadata()?)?;
        if FileFingerprint::of_path(&path)? != fingerprint {
            return Err(ServiceError::new(
                ErrorKind::SourceChanged,
                "file changed while opening",
            ));
        }
        let id = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
        self.inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .docs
            .insert(
                id,
                SparseIndex {
                    points: Vec::new(),
                    stride: MIN_INDEX_STRIDE,
                    indexed: 0,
                    newlines: 0,
                    total: fingerprint.len,
                },
            );
        Ok(PagedDocHandle {
            inner: Arc::new(PagedFile {
                id,
                doc_id,
                path,
                file,
                fingerprint,
                open_gen: id,
                generation: AtomicU64::new(id),
                has_cached_pages: AtomicBool::new(false),
                service: self.clone(),
            }),
        })
    }
    pub fn stats(&self) -> CacheStats {
        let cache = self.inner.cache.lock().unwrap_or_else(|e| e.into_inner());
        let indices = self.inner.indices.lock().unwrap_or_else(|e| e.into_inner());
        CacheStats {
            cached_bytes: cache.bytes,
            cache_budget: cache.budget,
            index_bytes: indices
                .docs
                .values()
                .map(|i| i.points.capacity() * std::mem::size_of::<Point>())
                .sum(),
            index_budget: indices.budget,
            pages: cache.pages.len(),
            sources: indices.docs.len(),
            active_index_jobs: INDEX_JOBS.active.load(Ordering::Acquire),
            peak_index_jobs: INDEX_JOBS.peak.load(Ordering::Relaxed),
            active_open_jobs: OPEN_JOBS.active.load(Ordering::Acquire),
            peak_open_jobs: OPEN_JOBS.peak.load(Ordering::Relaxed),
        }
    }
    fn invalidate(&self, id: u64, clear_pages: bool) {
        if clear_pages {
            let mut cache = self.inner.cache.lock().unwrap_or_else(|e| e.into_inner());
            let mut removed = 0;
            cache.pages.retain(|(source, _), page| {
                if *source == id {
                    removed += page.data.len();
                    false
                } else {
                    true
                }
            });
            cache.bytes -= removed;
        }
        self.inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .docs
            .remove(&id);
    }
}
impl PagedDocHandle {
    pub fn doc_id(&self) -> DocumentId {
        self.inner.doc_id
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    pub fn len(&self) -> u64 {
        self.inner.fingerprint.len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn fingerprint(&self) -> &FileFingerprint {
        &self.inner.fingerprint
    }
    pub fn source_generation(&self) -> SourceGeneration {
        SourceGeneration(self.inner.generation.load(Ordering::Acquire))
    }
    /// Checks both the open descriptor and the path (including symlink replacement).
    /// Metadata detects ordinary changes; it is not an adversarial filesystem snapshot.
    pub fn validate(&self, generation: SourceGeneration) -> Result<(), ServiceError> {
        self.ensure_generation(generation)?;
        let current_handle = self
            .inner
            .file
            .metadata()
            .map_err(ServiceError::from)
            .and_then(|m| FileFingerprint::of_metadata(&m));
        let current_path = FileFingerprint::of_path(&self.inner.path);
        if current_handle.as_ref().ok() != Some(&self.inner.fingerprint)
            || current_path.as_ref().ok() != Some(&self.inner.fingerprint)
        {
            return Err(self.trip_invalidation(
                "source was replaced, modified, appended, truncated, or removed",
            ));
        }
        Ok(())
    }
    fn ensure_generation(&self, generation: SourceGeneration) -> Result<(), ServiceError> {
        if generation.0 != self.inner.open_gen || generation != self.source_generation() {
            Err(ServiceError::new(
                ErrorKind::SourceChanged,
                "source generation expired; reopen the file",
            ))
        } else {
            Ok(())
        }
    }
    fn trip_invalidation(&self, message: &'static str) -> ServiceError {
        if self.inner.generation.load(Ordering::Acquire) == self.inner.open_gen {
            let next = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
            let _ = self.inner.generation.compare_exchange(
                self.inner.open_gen,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        // Always lock/clear the cache here: an insertion that passed its generation
        // check before the CAS must finish before this removal can complete.
        self.inner.service.invalidate(self.inner.id, true);
        ServiceError::new(ErrorKind::SourceChanged, message)
    }
    /// Uncached sequential read; caller owns a bounded reusable buffer.
    pub fn read_at(
        &self,
        offset: u64,
        buffer: &mut [u8],
        generation: SourceGeneration,
    ) -> Result<usize, ServiceError> {
        self.validate(generation)?;
        let done = self.read_raw(offset, buffer, generation)?;
        self.validate(generation)?;
        Ok(done)
    }
    // Top-level viewport/index/search operations bracket reads with identity checks.
    fn read_raw(
        &self,
        offset: u64,
        buffer: &mut [u8],
        generation: SourceGeneration,
    ) -> Result<usize, ServiceError> {
        self.ensure_generation(generation)?;
        let wanted = buffer.len().min(self.len().saturating_sub(offset) as usize);
        let mut done = 0;
        while done < wanted {
            #[cfg(unix)]
            let read = {
                use std::os::unix::fs::FileExt;
                self.inner
                    .file
                    .read_at(&mut buffer[done..wanted], offset + done as u64)
            };
            #[cfg(windows)]
            let read = {
                use std::os::windows::fs::FileExt;
                self.inner
                    .file
                    .seek_read(&mut buffer[done..wanted], offset + done as u64)
            };
            match read {
                Ok(0) => return Err(self.trip_invalidation("unexpected end of file")),
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        self.ensure_generation(generation)?;
        Ok(done)
    }
    fn page(
        &self,
        number: u64,
        generation: SourceGeneration,
    ) -> Result<Arc<Vec<u8>>, ServiceError> {
        self.ensure_generation(generation)?;
        let key = (self.inner.id, number);
        {
            let mut cache = self
                .inner
                .service
                .inner
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            cache.clock = cache.clock.wrapping_add(1);
            let clock = cache.clock;
            if let Some(page) = cache.pages.get_mut(&key) {
                page.touched = clock;
                return Ok(page.data.clone());
            }
        }
        let offset = number * PAGE_BYTES as u64;
        let mut bytes = vec![0; PAGE_BYTES.min(self.len().saturating_sub(offset) as usize)];
        self.read_raw(offset, &mut bytes, generation)?;
        let data = Arc::new(bytes);
        let mut cache = self
            .inner
            .service
            .inner
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.ensure_generation(generation)?;
        if let Some(page) = cache.pages.get(&key) {
            return Ok(page.data.clone());
        }
        while cache.bytes + data.len() > cache.budget {
            let oldest = cache
                .pages
                .iter()
                .min_by_key(|(_, page)| page.touched)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                if let Some(page) = cache.pages.remove(&oldest) {
                    cache.bytes -= page.data.len();
                }
            } else {
                break;
            }
        }
        cache.clock = cache.clock.wrapping_add(1);
        let touched = cache.clock;
        cache.bytes += data.len();
        cache.pages.insert(
            key,
            Page {
                data: data.clone(),
                touched,
            },
        );
        self.inner.has_cached_pages.store(true, Ordering::Relaxed);
        Ok(data)
    }
    fn bounded_range(
        &self,
        start: u64,
        len: usize,
        generation: SourceGeneration,
    ) -> Result<Vec<u8>, ServiceError> {
        let len = len.min(self.len().saturating_sub(start) as usize);
        let mut bytes = Vec::with_capacity(len);
        let mut at = start;
        while bytes.len() < len {
            let within = at as usize % PAGE_BYTES;
            let take = (len - bytes.len()).min(PAGE_BYTES - within);
            if take <= 8 {
                // Reuse an existing page, but never fault a whole third page for a peek.
                let cached = {
                    let cache = self
                        .inner
                        .service
                        .inner
                        .cache
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    cache
                        .pages
                        .get(&(self.inner.id, at / PAGE_BYTES as u64))
                        .map(|page| page.data.clone())
                };
                if let Some(page) = cached {
                    if within + take > page.len() {
                        return Err(self.trip_invalidation("short cached peek"));
                    }
                    bytes.extend_from_slice(&page[within..within + take]);
                } else {
                    let mut peek = [0u8; 8];
                    self.read_raw(at, &mut peek[..take], generation)?;
                    bytes.extend_from_slice(&peek[..take]);
                }
            } else {
                let page = self.page(at / PAGE_BYTES as u64, generation)?;
                if within + take > page.len() {
                    return Err(self.trip_invalidation("short cached page"));
                }
                bytes.extend_from_slice(&page[within..within + take]);
            }
            at += take as u64;
        }
        Ok(bytes)
    }
    pub fn index_status(&self) -> IndexStatus {
        let indices = self
            .inner
            .service
            .inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match indices.docs.get(&self.inner.id) {
            Some(i) if i.indexed >= i.total => IndexStatus::Complete {
                total_bytes: i.total,
                total_lines: i.newlines + 1,
            },
            Some(i) => IndexStatus::Indexing {
                indexed_bytes: i.indexed,
                total_bytes: i.total,
                known_lines: i.newlines + 1,
            },
            None => IndexStatus::Indexing {
                indexed_bytes: 0,
                total_bytes: self.len(),
                known_lines: 0,
            },
        }
    }
    pub fn index_checkpoint_count(&self) -> usize {
        self.inner
            .service
            .inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .docs
            .get(&self.inner.id)
            .map_or(0, |index| index.points.len())
    }
    /// Advances by at most 1 MiB. Busy means retain the task and retry later;
    /// at most two process-wide indexing reads and two reusable buffers are active.
    pub fn index_step(
        &self,
        generation: SourceGeneration,
        cancel: &CancelToken,
    ) -> Result<IndexStatus, ServiceError> {
        cancel.check()?;
        self.ensure_generation(generation)?;
        let (start, mut newlines) = self
            .inner
            .service
            .inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .docs
            .get(&self.inner.id)
            .map(|index| (index.indexed, index.newlines))
            .ok_or_else(|| ServiceError::new(ErrorKind::SourceChanged, "index invalidated"))?;
        if start == self.len() {
            self.validate(generation)?;
            return Ok(self.index_status());
        }
        let mut buffer = IndexBuffer::acquire()?;
        let count = self.read_at(start, &mut buffer.bytes, generation)?;
        let mut points = [Point { byte: 0, line: 0 }; 17];
        let mut point_count = 0;
        let mut consumed = 0;
        while consumed < count {
            cancel.check()?;
            let at = start + consumed as u64;
            let boundary = (at / MIN_INDEX_STRIDE + 1) * MIN_INDEX_STRIDE;
            let take = (boundary - at).min((count - consumed) as u64) as usize;
            newlines +=
                memchr::memchr_iter(b'\n', &buffer.bytes[consumed..consumed + take]).count() as u64;
            consumed += take;
            if start + consumed as u64 == boundary {
                points[point_count] = Point {
                    byte: boundary,
                    line: newlines,
                };
                point_count += 1;
            }
        }
        let mut indices = self
            .inner
            .service
            .inner
            .indices
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.ensure_generation(generation)?;
        let index = indices
            .docs
            .get_mut(&self.inner.id)
            .ok_or_else(|| ServiceError::new(ErrorKind::SourceChanged, "index invalidated"))?;
        if index.indexed != start {
            drop(indices);
            return Ok(self.index_status());
        }
        index.indexed += count as u64;
        index.newlines = newlines;
        for point in &points[..point_count] {
            indices.push_point(self.inner.id, *point);
        }
        drop(indices);
        Ok(self.index_status())
    }
    fn locate_line(
        &self,
        wanted: u64,
        generation: SourceGeneration,
        cancel: &CancelToken,
    ) -> Result<u64, ServiceError> {
        let point = {
            let indices = self
                .inner
                .service
                .inner
                .indices
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            indices
                .docs
                .get(&self.inner.id)
                .and_then(|i| i.points.iter().rev().find(|p| p.line < wanted).copied())
                .unwrap_or(Point { byte: 0, line: 0 })
        };
        if wanted == 0 {
            return Ok(0);
        }
        let mut at = point.byte;
        let mut line = point.line;
        let mut buf = vec![0; PAGE_BYTES];
        while at < self.len() {
            cancel.check()?;
            let n = self.read_at(at, &mut buf, generation)?;
            for newline in memchr::memchr_iter(b'\n', &buf[..n]) {
                line += 1;
                if line == wanted {
                    return Ok(at + newline as u64 + 1);
                }
            }
            at += n as u64;
        }
        Ok(self.len())
    }
    pub fn viewport(
        &self,
        request: ViewportRequest,
        cancel: &CancelToken,
    ) -> Result<ViewportSlice, ServiceError> {
        cancel.check()?;
        self.validate(request.source_gen)?;
        let requested = match request.anchor {
            ViewportAnchor::ByteOffset(n) => n.min(self.len()),
            ViewportAnchor::ApproxLine(n) => self.locate_line(n, request.source_gen, cancel)?,
        };
        let probe_start = requested.saturating_sub(4);
        let max_bytes = request.max_bytes.clamp(4, VIEWPORT_BYTES);
        let bytes = self.bounded_range(probe_start, max_bytes + 8, request.source_gen)?;
        let mut start = (requested - probe_start) as usize;
        let original_start = start;
        for _ in 0..3 {
            if start > 0 && start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
                start -= 1;
            } else {
                break;
            }
        }
        if start < bytes.len() && bytes[start] & 0xc0 == 0x80 {
            start = original_start;
        }
        if start > 0 && start < bytes.len() && bytes[start] == b'\n' && bytes[start - 1] == b'\r' {
            start -= 1;
        }
        let mut end = (start + max_bytes).min(bytes.len());
        let original_end = end;
        for _ in 0..3 {
            if end > start && end < bytes.len() && bytes[end] & 0xc0 == 0x80 {
                end -= 1;
            } else {
                break;
            }
        }
        // A run of invalid continuation bytes is not a valid UTF-8 suffix; let the
        // bounded lossy decoder consume it rather than returning a zero-byte page.
        if end < bytes.len() && bytes[end] & 0xc0 == 0x80 {
            end = original_end;
        }
        if end > start && end < bytes.len() && bytes[end - 1] == b'\r' && bytes[end] == b'\n' {
            end -= 1;
        }
        let (mut text, mut consumed, lossy) = decode_bounded(&bytes[start..end], max_bytes);
        if consumed > 0
            && start + consumed < bytes.len()
            && bytes[start + consumed - 1] == b'\r'
            && bytes[start + consumed] == b'\n'
        {
            text = Arc::from(&text[..text.len() - 1]);
            consumed -= 1;
        }
        end = start + consumed;
        let range = (probe_start + start as u64)..(probe_start + end as u64);
        let point = {
            let indices = self
                .inner
                .service
                .inner
                .indices
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            indices
                .docs
                .get(&self.inner.id)
                .filter(|i| i.indexed >= range.start)
                .and_then(|i| {
                    i.points
                        .iter()
                        .rev()
                        .find(|p| p.byte <= range.start)
                        .copied()
                        .or(Some(Point { byte: 0, line: 0 }))
                })
        };
        let first_line_number = if let Some(point) =
            point.filter(|p| range.start - p.byte <= INDEX_BLOCK_BYTES as u64)
        {
            let distance = (range.start - point.byte) as usize;
            let mut line = point.line;
            if distance <= PAGE_BYTES {
                let prefix = self.bounded_range(point.byte, distance, request.source_gen)?;
                line += memchr::memchr_iter(b'\n', &prefix).count() as u64;
            } else {
                // A sparse-index probe is a cancellable worker read, capped at 1 MiB
                // total and 64 KiB scratch. It never scans an unbounded logical line.
                let mut scratch = [0u8; PAGE_BYTES];
                let mut at = point.byte;
                while at < range.start {
                    cancel.check()?;
                    let take = (range.start - at).min(PAGE_BYTES as u64) as usize;
                    self.read_raw(at, &mut scratch[..take], request.source_gen)?;
                    line += memchr::memchr_iter(b'\n', &scratch[..take]).count() as u64;
                    at += take as u64;
                }
            }
            Some(line)
        } else if range.start == 0 {
            Some(0)
        } else {
            None
        };
        let is_line_fragment = (range.start > 0 && (start == 0 || bytes[start - 1] != b'\n'))
            || (range.end < self.len() && (end == start || bytes[end - 1] != b'\n'));
        self.validate(request.source_gen)?;
        Ok(ViewportSlice {
            source_gen: request.source_gen,
            byte_range: range,
            text,
            first_line_number,
            is_line_fragment,
            lossy,
            index_status: self.index_status(),
        })
    }
}

fn decode_bounded(bytes: &[u8], limit: usize) -> (Arc<str>, usize, bool) {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return (Arc::from(text), bytes.len(), false);
    }
    let mut text = String::with_capacity(limit);
    let mut consumed = 0;
    while consumed < bytes.len() && text.len() < limit {
        let remaining = &bytes[consumed..];
        let (valid, invalid) = match std::str::from_utf8(remaining) {
            Ok(_) => (remaining.len(), 0),
            Err(error) => (
                error.valid_up_to(),
                error
                    .error_len()
                    .unwrap_or(remaining.len() - error.valid_up_to()),
            ),
        };
        // valid_up_to always denotes a complete UTF-8 prefix.
        if let Ok(prefix) = std::str::from_utf8(&remaining[..valid]) {
            let mut take = prefix.len().min(limit - text.len());
            while !prefix.is_char_boundary(take) {
                take -= 1;
            }
            text.push_str(&prefix[..take]);
            consumed += take;
            if take < valid {
                break;
            }
        }
        if invalid == 0 {
            break;
        }
        if text.len() + 3 > limit {
            break;
        }
        text.push('\u{fffd}');
        consumed += invalid;
    }
    (Arc::from(text), consumed, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mid_read_eof_trips_generation_and_evicts_cached_data() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("source");
        std::fs::write(&path, vec![b'x'; 128 * 1024]).unwrap();
        let paging = PagingService::with_budgets(128 * 1024, 1024);
        let doc = paging.open(DocumentId::new(), &path).unwrap();
        let generation = doc.source_generation();
        doc.page(0, generation).unwrap();
        assert_eq!(paging.stats().cached_bytes, PAGE_BYTES);
        // Model truncation after the top-level metadata check and before pread.
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(0)
            .unwrap();
        let error = doc.read_raw(0, &mut [0; 8], generation).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SourceChanged);
        assert_ne!(doc.source_generation(), generation);
        assert_eq!(paging.stats().cached_bytes, 0);
        assert_eq!(paging.stats().index_bytes, 0);
    }

    #[test]
    fn sparse_index_uses_available_budget_beyond_1024_checkpoints() {
        let mut indices = Indices {
            docs: HashMap::new(),
            budget: 16 * 1024 * 1024,
        };
        indices.docs.insert(
            1,
            SparseIndex {
                points: Vec::new(),
                stride: MIN_INDEX_STRIDE,
                indexed: 0,
                newlines: 0,
                total: 256 * 1024 * 1024,
            },
        );
        for number in 1..=4096 {
            indices.push_point(
                1,
                Point {
                    byte: number * MIN_INDEX_STRIDE,
                    line: number,
                },
            );
        }
        let index = indices.docs.get(&1).unwrap();
        assert_eq!(index.points.len(), 4096);
        assert_eq!(index.stride, MIN_INDEX_STRIDE);
        assert!(index.points.capacity() * std::mem::size_of::<Point>() <= indices.budget);
    }

    #[test]
    fn index_buffers_and_open_permits_are_bounded_and_reusable() {
        let first = IndexBuffer::acquire().unwrap();
        let pointer = first.bytes.as_ptr();
        let second = IndexBuffer::acquire().unwrap();
        assert!(matches!(
            IndexBuffer::acquire(),
            Err(ServiceError {
                kind: ErrorKind::Busy,
                ..
            })
        ));
        drop(first);
        let reused = IndexBuffer::acquire().unwrap();
        assert_eq!(reused.bytes.as_ptr(), pointer);
        drop(reused);
        drop(second);
        let first = acquire_open_permit().unwrap();
        let second = acquire_open_permit().unwrap();
        assert!(matches!(
            acquire_open_permit(),
            Err(ServiceError {
                kind: ErrorKind::Busy,
                ..
            })
        ));
        drop(first);
        drop(second);
    }
}
