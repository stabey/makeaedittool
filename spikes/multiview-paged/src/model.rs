use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

pub use editor::paged_view::{
    DisplaySegment, MAX_DISPLAY_SEGMENT_CHARS, PAGE_STEP_BYTES, build_display_segments,
};
use editor_services::{
    CancelToken, DocMeta, DocumentId, IndexStatus, LifecycleGen, PagedDocHandle, ReadOnlyReason,
    SourceGeneration, VIEWPORT_BYTES, ViewId, ViewState, ViewportAnchor, ViewportRequest,
    ViewportSlice,
};
use gpui_kit::component::VirtualListScrollHandle;
use gpui_kit::{Pixels, ScrollStrategy, Size, px, size};

pub const SEGMENT_ROW_HEIGHT_PX: f32 = 20.0;
pub const SEGMENT_ROW_MEASURE_WIDTH_PX: f32 = 1600.0;

/// Tag carried on every async background `viewport()` task so foreground completion handling
/// enforces document identity, per-view request supersession, and source-generation fences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewportCompletionTag {
    pub doc_id: DocumentId,
    pub view_id: ViewId,
    pub req_seq: u64,
    pub expected_source_gen: SourceGeneration,
}

/// Per-document authority shared across all viewports attached to the same backing file:
/// owns the sole `PagedDocHandle` (`1` OS `File`, `1` `SparseIndex` entry, `1` shared cache source),
/// the cooperative indexing lifecycle, and document-wide `SourceChanged` invalidation state.
pub struct PagedSourceState {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub meta: DocMeta,
    pub handle: PagedDocHandle,
    pub source_gen: SourceGeneration,
    pub readonly_reason: ReadOnlyReason,
    pub index_status: IndexStatus,
    pub source_error: Option<String>,
    pub source_change_count: u64,
    pub index_cancel: CancelToken,
    pub indexer_in_flight: bool,
}

impl Drop for PagedSourceState {
    fn drop(&mut self) {
        self.index_cancel.cancel();
    }
}

impl PagedSourceState {
    pub fn new(
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        mut meta: DocMeta,
        handle: PagedDocHandle,
        source_gen: SourceGeneration,
        readonly_reason: ReadOnlyReason,
    ) -> Self {
        meta.paged = true;
        meta.readonly_reason = Some(readonly_reason.clone());
        let index_status = handle.index_status();
        Self {
            doc_id,
            lifecycle_gen,
            meta,
            handle,
            source_gen,
            readonly_reason,
            index_status,
            source_error: None,
            source_change_count: 0,
            index_cancel: CancelToken::new(),
            indexer_in_flight: false,
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.handle.len()
    }
}

/// Per-viewport state owned independently by each mounted pane/view over a shared `PagedSourceState`.
/// Dropping a `PagedViewportState` cancels ONLY its own in-flight `viewport_cancel` token and never
/// cancels `PagedSourceState::index_cancel` or releases the shared `PagedDocHandle`.
pub struct PagedViewportState {
    pub view_id: ViewId,
    pub slot_index: usize,
    pub current_anchor_byte: u64,
    pub current_slice: Option<ViewportSlice>,
    pub cached_segments: Arc<Vec<DisplaySegment>>,
    pub segment_item_sizes: Rc<Vec<Size<Pixels>>>,
    pub scroll_handle: VirtualListScrollHandle,
    pub highlighted_byte_range: Option<Range<u64>>,
    pub viewport_error: Option<String>,
    pub is_loading: bool,
    pub viewport_req_seq: u64,
    pub viewport_cancel: Option<CancelToken>,
    pub completed_requests: u64,
    pub superseded_cancels: u64,
}

impl Drop for PagedViewportState {
    fn drop(&mut self) {
        if let Some(cancel) = self.viewport_cancel.take() {
            cancel.cancel();
        }
    }
}

impl PagedViewportState {
    pub fn new(
        view_id: ViewId,
        slot_index: usize,
        initial_anchor_byte: u64,
        max_byte: u64,
    ) -> Self {
        Self {
            view_id,
            slot_index,
            current_anchor_byte: initial_anchor_byte.min(max_byte),
            current_slice: None,
            cached_segments: Arc::new(Vec::new()),
            segment_item_sizes: Rc::new(Vec::new()),
            scroll_handle: VirtualListScrollHandle::new(),
            highlighted_byte_range: None,
            viewport_error: None,
            is_loading: false,
            viewport_req_seq: 0,
            viewport_cancel: None,
            completed_requests: 0,
            superseded_cancels: 0,
        }
    }

    pub fn prepare_request(
        &mut self,
        doc_id: DocumentId,
        source_gen: SourceGeneration,
        anchor: ViewportAnchor,
    ) -> (ViewportCompletionTag, ViewportRequest, CancelToken) {
        if let Some(prev) = self.viewport_cancel.take() {
            prev.cancel();
            self.superseded_cancels = self.superseded_cancels.saturating_add(1);
        }
        self.viewport_req_seq = self.viewport_req_seq.wrapping_add(1);
        let cancel = CancelToken::new();
        self.viewport_cancel = Some(cancel.clone());
        self.is_loading = true;
        self.viewport_error = None;

        let tag = ViewportCompletionTag {
            doc_id,
            view_id: self.view_id,
            req_seq: self.viewport_req_seq,
            expected_source_gen: source_gen,
        };
        let req = ViewportRequest {
            source_gen,
            anchor,
            max_bytes: VIEWPORT_BYTES,
        };
        (tag, req, cancel)
    }

    pub fn apply_hydrated_slice(&mut self, slice: ViewportSlice) {
        self.is_loading = false;
        self.viewport_cancel = None;
        self.viewport_error = None;
        self.current_anchor_byte = slice.byte_range.start;

        let segs = build_display_segments(&slice);
        let row_size = size(px(SEGMENT_ROW_MEASURE_WIDTH_PX), px(SEGMENT_ROW_HEIGHT_PX));
        self.segment_item_sizes = Rc::new(vec![row_size; segs.len()]);
        self.cached_segments = Arc::new(segs);
        self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
        self.current_slice = Some(slice);
        self.completed_requests = self.completed_requests.saturating_add(1);
    }

    pub fn visible_byte_range(&self) -> Range<u64> {
        self.current_slice
            .as_ref()
            .map(|s| s.byte_range.clone())
            .unwrap_or(self.current_anchor_byte..self.current_anchor_byte)
    }

    pub fn next_page_offset(&self, total_bytes: u64) -> u64 {
        if let Some(slice) = &self.current_slice {
            if slice.byte_range.end >= total_bytes {
                return slice.byte_range.start;
            }
            (slice.byte_range.start + PAGE_STEP_BYTES).min(total_bytes.saturating_sub(1))
        } else {
            (self.current_anchor_byte + PAGE_STEP_BYTES).min(total_bytes)
        }
    }

    pub fn prev_page_offset(&self) -> u64 {
        self.current_anchor_byte.saturating_sub(PAGE_STEP_BYTES)
    }

    pub fn end_page_offset(total_bytes: u64) -> u64 {
        total_bytes.saturating_sub(VIEWPORT_BYTES as u64 / 2)
    }

    pub fn capture_view_state(&self) -> ViewState {
        ViewState {
            cursor_byte: self.current_anchor_byte,
            scroll_byte: self.current_anchor_byte,
        }
    }

    pub fn continuation_segment_count(&self) -> usize {
        self.cached_segments
            .iter()
            .filter(|seg| seg.is_continuation)
            .count()
    }
}
