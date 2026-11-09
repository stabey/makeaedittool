use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded};

use editor_services::{
    CacheStats, CancelToken, ErrorKind, IndexStatus, PagingService, SearchService, SearchTask,
    ServiceError, ViewId, ViewportAnchor, ViewportSlice,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
    v_virtual_list,
};
use gpui_kit::{
    AnyElement, App, Bounds, Context, ElementId, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Pixels, Render, ScrollStrategy, Styled as _, Window, canvas,
    div, prelude::FluentBuilder as _, px,
};

use crate::model::{PagedSourceState, PagedViewportState, ViewportCompletionTag};
use crate::report::ResourceSnapshot;

pub const PAGING_CACHE_BUDGET_BYTES: usize = 64 * 1024 * 1024;
pub const PAGING_INDEX_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Test-synchronization gate allowing the experiment runner to hold a REAL `cx.spawn` async
/// viewport continuation *after* its background `PagedDocHandle::viewport` read has completed
/// and dropped its worker `PagedDocHandle` (blocking zero worker threads), then release it
/// across request supersession or `SourceChanged` invalidation to verify completion fences.
pub struct ContinuationHoldGate {
    pub entered_tx: Sender<()>,
    pub release_rx: Receiver<()>,
    pub delivered_tx: Sender<()>,
}

pub fn make_continuation_hold_gate()
-> (ContinuationHoldGate, Receiver<()>, Sender<()>, Receiver<()>) {
    let (entered_tx, entered_rx) = bounded(1);
    let (release_tx, release_rx) = bounded(1);
    let (delivered_tx, delivered_rx) = bounded(1);
    (
        ContinuationHoldGate {
            entered_tx,
            release_rx,
            delivered_tx,
        },
        entered_rx,
        release_tx,
        delivered_rx,
    )
}

struct BgOpGuard(Arc<AtomicUsize>);

impl BgOpGuard {
    fn acquire(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::AcqRel);
        Self(counter.clone())
    }
}

impl Drop for BgOpGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

#[inline]
pub fn view_elem_id(tag: [u8; 4], id: ViewId) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4..].copy_from_slice(&id.0.to_be_bytes());
    ElementId::from(bytes)
}

/// Real native GPUI entity hosting `N` (`1, 2, 4, 8`) independent `PagedViewportState`s over one
/// shared `PagedSourceState` (`1 PagedDocHandle`, `1 SparseIndex`, `1 PagingService` budget domain).
///
/// Threading invariant:
/// - GPUI foreground thread owns `MultiviewPagedHarness`, `PagedSourceState` metadata/status,
///   all `PagedViewportState` instances, `VirtualListScrollHandle`s, focus, and rendering.
/// - Zero filesystem I/O ever runs on the GPUI foreground thread.
pub struct MultiviewPagedHarness {
    pub paging: PagingService,
    pub search: SearchService,
    pub active_search_task: Option<SearchTask>,

    pub source: Option<PagedSourceState>,
    pub views: Vec<PagedViewportState>,
    pub active_view: Option<ViewId>,
    pub focus_handle: FocusHandle,

    // Completion fence & frame observability counters
    in_flight_bg_ops: Arc<AtomicUsize>,
    pub frames_rendered: u64,
    pub cancelled_completions: u64,
    pub stale_doc_dropped: u64,
    pub stale_source_gen_dropped: u64,
    pub stale_req_seq_dropped: u64,
    // Per-frame prepaint / paint probes populated by real GPUI layout & draw
    pub measured_root_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub measured_leaf_bounds: Rc<RefCell<Vec<(ViewId, Bounds<Pixels>)>>>,
    pub visible_ranges_log: Rc<RefCell<HashMap<ViewId, Range<usize>>>>,
    pub prepainted_error_views: Rc<RefCell<Vec<ViewId>>>,
    pub rendered_highlight_views: Rc<RefCell<Vec<(ViewId, Range<u64>)>>>,
    pub rendered_gutter_samples: Rc<RefCell<HashMap<ViewId, String>>>,
}

impl Focusable for MultiviewPagedHarness {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl MultiviewPagedHarness {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);

        Self {
            paging: PagingService::with_budgets(
                PAGING_CACHE_BUDGET_BYTES,
                PAGING_INDEX_BUDGET_BYTES,
            ),
            search: SearchService::with_limit(4),
            active_search_task: None,
            source: None,
            views: Vec::new(),
            active_view: None,
            focus_handle,
            in_flight_bg_ops: Arc::new(AtomicUsize::new(0)),
            frames_rendered: 0,
            cancelled_completions: 0,
            stale_doc_dropped: 0,
            stale_source_gen_dropped: 0,
            stale_req_seq_dropped: 0,
            measured_root_bounds: Rc::new(Cell::new(None)),
            measured_leaf_bounds: Rc::new(RefCell::new(Vec::new())),
            visible_ranges_log: Rc::new(RefCell::new(HashMap::new())),
            prepainted_error_views: Rc::new(RefCell::new(Vec::new())),
            rendered_highlight_views: Rc::new(RefCell::new(Vec::new())),
            rendered_gutter_samples: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn in_flight_bg_ops(&self) -> usize {
        self.in_flight_bg_ops.load(Ordering::Acquire)
    }

    pub fn install_source(&mut self, source: PagedSourceState, cx: &mut Context<Self>) {
        self.teardown_all(cx);
        self.source = Some(source);
        cx.notify();
    }

    /// Mounts `count` (`1, 2, 4, 8`) independent viewports over the currently installed shared
    /// `PagedSourceState` without duplicating `PagedDocHandle` opens or `SparseIndex` state.
    pub fn attach_views(
        &mut self,
        initial_anchors: &[u64],
        cx: &mut Context<Self>,
    ) -> Result<Vec<ViewId>, String> {
        let Some(source) = &self.source else {
            return Err("cannot attach_views without an installed PagedSourceState".to_string());
        };
        let max_byte = source.total_bytes();
        self.views.clear();
        self.visible_ranges_log.borrow_mut().clear();
        self.measured_leaf_bounds.borrow_mut().clear();
        self.prepainted_error_views.borrow_mut().clear();
        self.rendered_highlight_views.borrow_mut().clear();
        self.rendered_gutter_samples.borrow_mut().clear();

        let mut ids = Vec::with_capacity(initial_anchors.len());
        for (slot_ix, &anchor) in initial_anchors.iter().enumerate() {
            let id = ViewId::new();
            ids.push(id);
            self.views
                .push(PagedViewportState::new(id, slot_ix, anchor, max_byte));
        }
        self.active_view = ids.first().copied();
        cx.notify();
        Ok(ids)
    }

    pub fn view_by_id(&self, view_id: ViewId) -> Option<&PagedViewportState> {
        self.views.iter().find(|v| v.view_id == view_id)
    }

    pub fn view_by_id_mut(&mut self, view_id: ViewId) -> Option<&mut PagedViewportState> {
        self.views.iter_mut().find(|v| v.view_id == view_id)
    }

    /// Switches active view focus; per ADR-009 search highlight ownership, switching active target
    /// clears `highlighted_byte_range` on the previously active view so highlights stay view-local.
    pub fn activate_view(&mut self, view_id: ViewId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.views.iter().any(|v| v.view_id == view_id) {
            return;
        }
        let prev = self.active_view;
        if prev != Some(view_id) {
            for v in &mut self.views {
                if v.view_id != view_id {
                    v.highlighted_byte_range = None;
                }
            }
            self.active_view = Some(view_id);
            cx.notify();
        }
        if window.focused(cx).as_ref() != Some(&self.focus_handle) {
            self.focus_handle.focus(window, cx);
        }
    }

    pub fn set_active_view_highlight(
        &mut self,
        match_range: Option<Range<u64>>,
        cx: &mut Context<Self>,
    ) {
        let active = self.active_view;
        for v in &mut self.views {
            if Some(v.view_id) == active {
                v.highlighted_byte_range = match_range.clone();
            } else {
                v.highlighted_byte_range = None;
            }
        }
        cx.notify();
    }

    pub fn cancel_active_search(&mut self) {
        if let Some(task) = self.active_search_task.take() {
            task.cancel.cancel();
        }
    }

    /// Dispatches a background `PagedDocHandle::viewport` read tagged with
    /// `(doc_id, view_id, req_seq, expected_source_gen)`. Zero foreground file reads/writes.
    pub fn request_viewport(
        &mut self,
        view_id: ViewId,
        anchor: ViewportAnchor,
        enable_auto_indexer: bool,
        cx: &mut Context<Self>,
    ) -> Result<(ViewportCompletionTag, CancelToken), String> {
        self.request_viewport_with_gate(view_id, anchor, enable_auto_indexer, None, cx)
    }

    pub fn request_viewport_with_gate(
        &mut self,
        view_id: ViewId,
        anchor: ViewportAnchor,
        enable_auto_indexer: bool,
        hold_gate: Option<ContinuationHoldGate>,
        cx: &mut Context<Self>,
    ) -> Result<(ViewportCompletionTag, CancelToken), String> {
        let Some(source) = &self.source else {
            return Err("request_viewport called with no PagedSourceState".to_string());
        };
        if source.source_error.is_some() {
            return Err("request_viewport called on invalidated PagedSourceState".to_string());
        }
        let doc_id = source.doc_id;
        let source_gen = source.source_gen;
        let handle = source.handle.clone();

        let Some(view) = self.views.iter_mut().find(|v| v.view_id == view_id) else {
            return Err(format!("unknown view_id {}", view_id.key()));
        };
        let (tag, req, cancel) = view.prepare_request(doc_id, source_gen, anchor);
        let returned_cancel = cancel.clone();
        let op_guard = BgOpGuard::acquire(&self.in_flight_bg_ops);
        cx.notify();

        cx.spawn(async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let res = loop {
                        match handle.viewport(req.clone(), &cancel) {
                            Err(e) if e.kind == ErrorKind::Busy && !cancel.is_cancelled() => {
                                std::thread::sleep(Duration::from_millis(4));
                            }
                            other => break other,
                        }
                    };
                    // Drop worker PagedDocHandle clone BEFORE returning so BgOpGuard decrement
                    // strictly post-dates worker handle release.
                    drop(handle);
                    res
                })
                .await;
            drop(op_guard);

            let delivered_tx = if let Some(gate) = hold_gate {
                let _ = gate.entered_tx.try_send(());
                loop {
                    match gate.release_rx.try_recv() {
                        Ok(()) | Err(TryRecvError::Disconnected) => break,
                        Err(TryRecvError::Empty) => {
                            cx.background_executor()
                                .timer(Duration::from_millis(2))
                                .await;
                        }
                    }
                }
                Some(gate.delivered_tx)
            } else {
                None
            };

            let _ = weak.update(cx, |this, cx| {
                this.complete_viewport_fenced(tag, result, enable_auto_indexer, cx);
            });
            if let Some(tx) = delivered_tx {
                let _ = tx.try_send(());
            }
        })
        .detach();

        Ok((tag, returned_cancel))
    }

    /// Applies a viewport completion behind `(doc_id, view_id, req_seq, expected_source_gen)` and
    /// live `PagedDocHandle::source_generation()` fences.
    pub fn complete_viewport_fenced(
        &mut self,
        tag: ViewportCompletionTag,
        result: Result<ViewportSlice, ServiceError>,
        enable_auto_indexer: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = &self.source else {
            self.stale_doc_dropped = self.stale_doc_dropped.saturating_add(1);
            return;
        };
        if source.doc_id != tag.doc_id {
            self.stale_doc_dropped = self.stale_doc_dropped.saturating_add(1);
            return;
        }

        // Classify explicit cancellations before any stale-generation or stale-seq counters so
        // cancelled requests never masquerade as dropped stale Ok completions.
        if matches!(&result, Err(err) if err.kind == ErrorKind::Cancelled) {
            self.cancelled_completions = self.cancelled_completions.saturating_add(1);
            return;
        }

        // Query the live atomic generation on PagedFile: another worker may have tripped
        // `PagedDocHandle::trip_invalidation` before its foreground continuation ran.
        let live_gen = source.handle.source_generation();

        if let Err(err) = &result
            && err.kind == ErrorKind::SourceChanged
        {
            if source.source_error.is_none() {
                self.trip_all_views_source_changed(err.message.clone(), cx);
            }
            return;
        }

        if source.source_error.is_none()
            && (live_gen != source.source_gen
                || matches!(&result, Ok(slice) if slice.source_gen != live_gen))
        {
            self.trip_all_views_source_changed(
                "shared PagedDocHandle generation advanced on worker; source invalidated"
                    .to_string(),
                cx,
            );
        }

        let Some(source) = &self.source else {
            self.stale_doc_dropped = self.stale_doc_dropped.saturating_add(1);
            return;
        };
        let live_gen_now = source.handle.source_generation();
        if source.source_error.is_some()
            || tag.expected_source_gen != source.source_gen
            || tag.expected_source_gen != live_gen_now
        {
            self.stale_source_gen_dropped = self.stale_source_gen_dropped.saturating_add(1);
            return;
        }

        let Some(view_ix) = self.views.iter().position(|v| v.view_id == tag.view_id) else {
            self.stale_req_seq_dropped = self.stale_req_seq_dropped.saturating_add(1);
            return;
        };
        if self.views[view_ix].viewport_req_seq != tag.req_seq {
            self.stale_req_seq_dropped = self.stale_req_seq_dropped.saturating_add(1);
            return;
        }

        match result {
            Ok(slice) => {
                if let Some(src) = &mut self.source {
                    src.index_status = slice.index_status;
                }
                self.views[view_ix].apply_hydrated_slice(slice);
                if enable_auto_indexer {
                    self.ensure_cooperative_indexer_running(cx);
                }
                cx.notify();
            }
            Err(err) => {
                self.views[view_ix].is_loading = false;
                self.views[view_ix].viewport_cancel = None;
                self.views[view_ix].viewport_error = Some(err.message);
                cx.notify();
            }
        }
    }

    /// Trips shared `SourceChanged` invalidation across the single `PagedSourceState` and ALL
    /// attached viewports simultaneously.
    pub fn trip_all_views_source_changed(&mut self, reason: String, cx: &mut Context<Self>) {
        let Some(source) = &mut self.source else {
            return;
        };
        if source.source_error.is_none() {
            source.source_error = Some(reason);
            source.source_change_count = source.source_change_count.saturating_add(1);
        }
        source.index_cancel.cancel();
        source.indexer_in_flight = false;
        // Reflect any bumped atomic generation from `PagedDocHandle::trip_invalidation` so late
        // completions holding the old `expected_source_gen` hit the generation fence immediately.
        source.source_gen = source.handle.source_generation();

        self.cancel_active_search();

        for v in &mut self.views {
            if let Some(cancel) = v.viewport_cancel.take() {
                cancel.cancel();
            }
            v.is_loading = false;
            v.highlighted_byte_range = None;
            v.current_slice = None;
            v.cached_segments = Arc::new(Vec::new());
            v.segment_item_sizes = Rc::new(Vec::new());
        }
        cx.notify();
    }

    /// Starts at most 1 cooperative background `index_step` loop per `PagedSourceState`.
    /// Yields whenever ANY attached viewport has `is_loading == true` so background 1 MiB index
    /// blocks never contend with foreground-triggered 64 KiB viewport reads.
    pub fn ensure_cooperative_indexer_running(&mut self, cx: &mut Context<Self>) {
        let Some(source) = &mut self.source else {
            return;
        };
        if source.indexer_in_flight
            || source.source_error.is_some()
            || matches!(source.index_status, IndexStatus::Complete { .. })
        {
            return;
        }
        source.indexer_in_flight = true;
        let doc_id = source.doc_id;
        let expected_gen = source.source_gen;
        let handle = source.handle.clone();
        let cancel = source.index_cancel.clone();
        let op_guard = BgOpGuard::acquire(&self.in_flight_bg_ops);

        cx.spawn(async move |weak, cx| {
            let handle_slot = Some(handle);
            loop {
                if cancel.is_cancelled() {
                    let _ = weak.update(cx, |this, _| {
                        if let Some(src) = &mut this.source
                            && src.doc_id == doc_id
                        {
                            src.indexer_in_flight = false;
                        }
                    });
                    break;
                }

                let gate = weak
                    .update(cx, |this, _| {
                        let Some(src) = &mut this.source else {
                            return None;
                        };
                        if src.doc_id != doc_id
                            || src.source_error.is_some()
                            || src.source_gen != expected_gen
                            || src.handle.source_generation() != expected_gen
                            || matches!(src.index_status, IndexStatus::Complete { .. })
                            || this.views.is_empty()
                        {
                            src.indexer_in_flight = false;
                            return None;
                        }
                        let any_loading = this.views.iter().any(|v| v.is_loading);
                        Some(!any_loading)
                    })
                    .ok()
                    .flatten();

                match gate {
                    None => break,
                    Some(false) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(8))
                            .await;
                        continue;
                    }
                    Some(true) => {}
                }

                let Some(handle_step) = handle_slot.as_ref().cloned() else {
                    break;
                };
                let cancel_step = cancel.clone();
                let step_res = cx
                    .background_executor()
                    .spawn(async move {
                        let res = handle_step.index_step(expected_gen, &cancel_step);
                        drop(handle_step);
                        res
                    })
                    .await;

                let mut sleep_ms = 0u64;
                let keep_going = weak
                    .update(cx, |this, cx| {
                        let Some(src) = &mut this.source else {
                            return false;
                        };
                        if src.doc_id != doc_id {
                            return false;
                        }
                        if src.source_error.is_some() || src.source_gen != expected_gen {
                            src.indexer_in_flight = false;
                            return false;
                        }
                        match step_res {
                            Ok(status) => {
                                src.index_status = status;
                                let done = matches!(status, IndexStatus::Complete { .. });
                                if done {
                                    src.indexer_in_flight = false;
                                }
                                cx.notify();
                                !done
                            }
                            Err(err) if err.kind == ErrorKind::Busy => {
                                sleep_ms = 12;
                                true
                            }
                            Err(err) if err.kind == ErrorKind::Cancelled => {
                                src.indexer_in_flight = false;
                                false
                            }
                            Err(err) if err.kind == ErrorKind::SourceChanged => {
                                this.trip_all_views_source_changed(err.message, cx);
                                false
                            }
                            Err(err) => {
                                src.source_error = Some(err.message);
                                src.indexer_in_flight = false;
                                cx.notify();
                                false
                            }
                        }
                    })
                    .unwrap_or(false);

                if !keep_going {
                    break;
                }
                if sleep_ms > 0 {
                    cx.background_executor()
                        .timer(Duration::from_millis(sleep_ms))
                        .await;
                }
            }
            // Drop the indexer's PagedDocHandle clone strictly BEFORE decrementing BgOpGuard.
            drop(handle_slot);
            drop(op_guard);
        })
        .detach();
    }

    /// Closes a single view by `ViewId`. Preserves `PagedSourceState` (`handle`, `index_cancel`,
    /// `SparseIndex`, and shared cached pages) while `>= 1` view remains open.
    pub fn close_view(&mut self, view_id: ViewId, cx: &mut Context<Self>) -> bool {
        let Some(pos) = self.views.iter().position(|v| v.view_id == view_id) else {
            return false;
        };
        let removed = self.views.remove(pos);
        drop(removed);
        self.visible_ranges_log.borrow_mut().remove(&view_id);
        self.rendered_gutter_samples.borrow_mut().remove(&view_id);
        if self.active_view == Some(view_id) {
            self.active_view = self.views.first().map(|v| v.view_id);
        }
        cx.notify();
        true
    }

    /// Closes all views AND drops `PagedSourceState` (`1 -> 0` lifecycle release).
    pub fn close_source_and_all_views(&mut self, cx: &mut Context<Self>) -> Option<CancelToken> {
        self.cancel_active_search();
        self.views.clear();
        self.active_view = None;
        self.visible_ranges_log.borrow_mut().clear();
        self.measured_leaf_bounds.borrow_mut().clear();
        self.prepainted_error_views.borrow_mut().clear();
        self.rendered_highlight_views.borrow_mut().clear();
        self.rendered_gutter_samples.borrow_mut().clear();
        let index_token = self.source.as_ref().map(|s| s.index_cancel.clone());
        self.source = None;
        cx.notify();
        index_token
    }

    pub fn teardown_all(&mut self, cx: &mut Context<Self>) {
        let _ = self.close_source_and_all_views(cx);
    }

    pub fn snapshot_resources(&self, phase: &'static str) -> ResourceSnapshot {
        let total_segments: usize = self.views.iter().map(|v| v.cached_segments.len()).sum();
        let total_item_sizes: usize = self.views.iter().map(|v| v.segment_item_sizes.len()).sum();
        let vis_log = self.visible_ranges_log.borrow();
        let total_visible: usize = self
            .views
            .iter()
            .filter_map(|v| vis_log.get(&v.view_id))
            .map(|r| r.len())
            .sum();
        let stats: CacheStats = self.paging.stats();
        ResourceSnapshot::from_stats(
            phase,
            self.views.len(),
            total_segments,
            total_item_sizes,
            total_visible,
            stats,
        )
    }

    fn render_status_header(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let stats = self.paging.stats();
        let (doc_badge, index_badge) = match &self.source {
            Some(src) => {
                let idx = match src.index_status {
                    IndexStatus::Indexing {
                        indexed_bytes,
                        total_bytes,
                        known_lines,
                    } => format!(
                        "indexing {} / {} B ({known_lines} lines)",
                        indexed_bytes, total_bytes
                    ),
                    IndexStatus::Complete {
                        total_bytes,
                        total_lines,
                    } => format!("index complete ({total_lines} lines, {total_bytes} B)"),
                };
                (
                    format!(
                        "doc={} gen={} err={}",
                        &src.doc_id.0.to_string()[..8.min(src.doc_id.0.to_string().len())],
                        src.source_gen.0,
                        src.source_error.is_some()
                    ),
                    idx,
                )
            }
            None => ("no-source".to_string(), "idle".to_string()),
        };

        h_flex()
            .id("multiview-paged-header")
            .h(px(28.0))
            .w_full()
            .px_3()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .bg(cx.theme().secondary.opacity(0.45))
            .border_b_1()
            .border_color(cx.theme().border)
            .text_xs()
            .child(format!(
                "Multiview Paged Harness · views={} · {doc_badge} · {index_badge}",
                self.views.len()
            ))
            .child(format!(
                "sources={} pages={} cached={} KiB",
                stats.sources,
                stats.pages,
                stats.cached_bytes / 1024
            ))
    }

    fn render_viewport_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.views.len();
        if count == 0 {
            return div()
                .id("multiview-empty-grid")
                .flex_1()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("0 viewports mounted (source released)")
                .into_any_element();
        }

        // Up to 2 rows x 4 columns so every leaf in a 1440x860 (or even 1152x769) window stays
        // strictly >= 240x160 logical pixels across N in {1, 2, 4, 8}.
        if count <= 2 {
            let mut row = h_flex()
                .id("multiview-row-single")
                .flex_1()
                .size_full()
                .min_w_0()
                .min_h_0();
            for v in &self.views {
                row = row.child(self.render_viewport_leaf(v, count, cx));
            }
            return row.into_any_element();
        }

        let top_count = count.div_ceil(2);
        let mut top_row = h_flex()
            .id("multiview-row-top")
            .flex_1()
            .w_full()
            .min_w_0()
            .min_h_0();
        for v in &self.views[..top_count] {
            top_row = top_row.child(self.render_viewport_leaf(v, count, cx));
        }

        let mut bottom_row = h_flex()
            .id("multiview-row-bottom")
            .flex_1()
            .w_full()
            .min_w_0()
            .min_h_0();
        for v in &self.views[top_count..] {
            bottom_row = bottom_row.child(self.render_viewport_leaf(v, count, cx));
        }

        v_flex()
            .id("multiview-grid-2row")
            .flex_1()
            .size_full()
            .min_w_0()
            .min_h_0()
            .child(top_row)
            .child(bottom_row)
            .into_any_element()
    }

    fn render_viewport_leaf(
        &self,
        pv: &PagedViewportState,
        pane_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let view_id = pv.view_id;
        let is_active = self.active_view == Some(view_id);
        let total_bytes = self
            .source
            .as_ref()
            .map_or(0, PagedSourceState::total_bytes);
        let shared_source_error = self.source.as_ref().and_then(|s| s.source_error.clone());
        let source_gen_val = self.source.as_ref().map_or(0, |s| s.source_gen.0);

        let leaves_rc = self.measured_leaf_bounds.clone();
        let leaf_probe = canvas(
            move |bounds, _window, _cx| {
                leaves_rc.borrow_mut().push((view_id, bounds));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();

        let range = pv.visible_byte_range();
        let at_start = range.start == 0;
        let at_end = range.end >= total_bytes;
        let prev_off = pv.prev_page_offset();
        let next_off = pv.next_page_offset(total_bytes);
        let end_off = PagedViewportState::end_page_offset(total_bytes);

        let toolbar = h_flex()
            .id(view_elem_id(*b"pgtb", view_id))
            .h(px(26.0))
            .w_full()
            .px_2()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .bg(cx.theme().secondary.opacity(0.32))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .text_color(if is_active {
                        cx.theme().primary
                    } else {
                        cx.theme().foreground
                    })
                    .child(format!("V{}", pv.slot_index)),
            )
            .child(
                Button::new(view_elem_id(*b"pgst", view_id))
                    .ghost()
                    .xsmall()
                    .label("⟪")
                    .disabled(at_start || shared_source_error.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ =
                            this.request_viewport(view_id, ViewportAnchor::ByteOffset(0), true, cx);
                    })),
            )
            .child(
                Button::new(view_elem_id(*b"pgpv", view_id))
                    .ghost()
                    .xsmall()
                    .label("Prev")
                    .disabled(at_start || shared_source_error.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = this.request_viewport(
                            view_id,
                            ViewportAnchor::ByteOffset(prev_off),
                            true,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new(view_elem_id(*b"pgnx", view_id))
                    .ghost()
                    .xsmall()
                    .label("Next")
                    .disabled(at_end || shared_source_error.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = this.request_viewport(
                            view_id,
                            ViewportAnchor::ByteOffset(next_off),
                            true,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new(view_elem_id(*b"pgen", view_id))
                    .ghost()
                    .xsmall()
                    .label("⟫")
                    .disabled(at_end || shared_source_error.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let _ = this.request_viewport(
                            view_id,
                            ViewportAnchor::ByteOffset(end_off),
                            true,
                            cx,
                        );
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .whitespace_nowrap()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("0x{:X} g{}", range.start, source_gen_val)),
            );

        let mono = cx.theme().mono_font_family.clone();

        let body = if let Some(err) = shared_source_error {
            let err_views_rc = self.prepainted_error_views.clone();
            let err_probe = canvas(
                move |_bounds, _window, _cx| {
                    err_views_rc.borrow_mut().push(view_id);
                },
                |_, (), _, _| {},
            )
            .absolute()
            .inset_0();

            div()
                .id(view_elem_id(*b"pger", view_id))
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .p_3()
                .font_family(mono)
                .text_xs()
                .text_color(cx.theme().danger)
                .child(err_probe)
                .child(format!("Shared source invalidated (SourceChanged): {err}"))
                .into_any_element()
        } else if let Some(vp_err) = &pv.viewport_error {
            div()
                .id(view_elem_id(*b"pgve", view_id))
                .flex_1()
                .min_h_0()
                .w_full()
                .p_3()
                .font_family(mono)
                .text_xs()
                .text_color(cx.theme().danger)
                .child(format!("Viewport read error: {vp_err}"))
                .into_any_element()
        } else if pv.current_slice.is_some() {
            let entity = cx.entity();
            let scroll_handle = pv.scroll_handle.clone();
            let item_sizes = pv.segment_item_sizes.clone();

            let vlist = v_virtual_list(
                entity,
                view_elem_id(*b"pgvl", view_id),
                item_sizes,
                move |this, visible_range: Range<usize>, _window, cx| {
                    this.visible_ranges_log
                        .borrow_mut()
                        .insert(view_id, visible_range.clone());

                    let Some(pv_now) = this.view_by_id(view_id) else {
                        return Vec::new();
                    };
                    let Some(slice_now) = &pv_now.current_slice else {
                        return Vec::new();
                    };
                    let slice_text: &str = &slice_now.text;
                    let active_match_range = pv_now.highlighted_byte_range.clone();
                    let row_mono = mono.clone();
                    let first_vis_ix = visible_range.start;
                    let gutter_samples = this.rendered_gutter_samples.clone();
                    let highlight_views = this.rendered_highlight_views.clone();

                    visible_range
                        .filter_map(|seg_ix| {
                            let seg = pv_now.cached_segments.get(seg_ix)?;
                            let seg_str = slice_text.get(seg.text_range.clone()).unwrap_or("");
                            let gutter_label = match &seg.exact_file_byte_range {
                                Some(exact) if seg.is_continuation => {
                                    format!("   ↳ @0x{:08X}", exact.start)
                                }
                                Some(exact) => match seg.logical_line_1based {
                                    Some(ln) => format!("{ln:>6} @0x{:08X}", exact.start),
                                    None => format!("     ~ @0x{:08X}", exact.start),
                                },
                                None => match seg.logical_line_1based {
                                    Some(ln) if !seg.is_continuation => format!("{ln:>6} ~lossy"),
                                    _ => "     ~ ~lossy".to_string(),
                                },
                            };

                            if seg_ix == first_vis_ix {
                                gutter_samples
                                    .borrow_mut()
                                    .insert(view_id, gutter_label.clone());
                            }

                            let overlaps_match =
                                seg.exact_file_byte_range.as_ref().and_then(|exact| {
                                    let m = active_match_range.as_ref()?;
                                    let s = m.start.max(exact.start);
                                    let e = m.end.min(exact.end);
                                    (s < e).then(|| {
                                        let rel_s = (s - exact.start) as usize;
                                        let rel_e = (e - exact.start) as usize;
                                        (
                                            rel_s.min(seg_str.len()),
                                            rel_e.min(seg_str.len()),
                                            m.clone(),
                                        )
                                    })
                                });

                            let text_cell = if let Some((ms, me, full_match)) = overlaps_match
                                && seg_str.is_char_boundary(ms)
                                && seg_str.is_char_boundary(me)
                            {
                                highlight_views.borrow_mut().push((view_id, full_match));
                                let pre = seg_str[..ms].to_string();
                                let mid = seg_str[ms..me].to_string();
                                let post = seg_str[me..].to_string();
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .child(pre)
                                    .child(
                                        div()
                                            .px_0p5()
                                            .rounded(px(2.0))
                                            .bg(cx.theme().primary.opacity(0.38))
                                            .font_semibold()
                                            .child(mid),
                                    )
                                    .child(post)
                                    .into_any_element()
                            } else {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .child(seg_str.to_string())
                                    .into_any_element()
                            };

                            Some(
                                h_flex()
                                    .id(("paged-seg", seg_ix))
                                    .h(px(20.0))
                                    .w_full()
                                    .px_1p5()
                                    .gap_2()
                                    .items_center()
                                    .font_family(row_mono.clone())
                                    .text_xs()
                                    .child(
                                        div()
                                            .w(px(136.0))
                                            .flex_shrink_0()
                                            .whitespace_nowrap()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(gutter_label),
                                    )
                                    .child(text_cell),
                            )
                        })
                        .collect()
                },
            )
            .track_scroll(&scroll_handle);

            div()
                .id(view_elem_id(*b"pgvc", view_id))
                .flex_1()
                .min_h_0()
                .min_w_0()
                .size_full()
                .child(vlist)
                .into_any_element()
        } else {
            div()
                .id(view_elem_id(*b"pgld", view_id))
                .flex_1()
                .min_h_0()
                .w_full()
                .p_3()
                .font_family(mono)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Loading 64 KiB viewport slice…")
                .into_any_element()
        };

        let show_focus_ring = pane_count > 1;
        let visual_frame = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .when(show_focus_ring && is_active, |el| {
                el.border_1().border_color(cx.theme().primary.opacity(0.75))
            })
            .when(show_focus_ring && !is_active, |el| {
                el.border_1().border_color(cx.theme().border.opacity(0.6))
            })
            .child(toolbar)
            .child(body);

        div()
            .id(view_elem_id(*b"pane", view_id))
            .relative()
            .flex_1()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(move |this, _ev, window, cx| {
                this.activate_view(view_id, window, cx);
            }))
            .child(leaf_probe)
            .child(visual_frame)
            .into_any_element()
    }
}

impl Render for MultiviewPagedHarness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.frames_rendered = self.frames_rendered.saturating_add(1);
        self.measured_leaf_bounds.borrow_mut().clear();
        self.prepainted_error_views.borrow_mut().clear();
        self.rendered_highlight_views.borrow_mut().clear();

        let root_cell = self.measured_root_bounds.clone();
        let root_probe = canvas(
            move |bounds, _window, _cx| {
                root_cell.set(Some(bounds));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();

        let header = self.render_status_header(cx);
        let grid = self.render_viewport_grid(cx);

        div()
            .id("multiview-paged-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(root_probe)
            .child(header)
            .child(grid)
    }
}

#[allow(dead_code)]
fn _assert_scroll_strategy_imported(_: ScrollStrategy) {}
