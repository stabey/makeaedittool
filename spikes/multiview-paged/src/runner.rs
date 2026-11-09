use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use editor_services::{
    CancelToken, DocMeta, DocumentId, ErrorKind, IndexStatus, LifecycleGen, OpenOptions,
    OpenOutcome, PagingService, QueryId, SearchEvent, SearchMatch, SearchRequest, SearchTarget,
    TryRecvError, ViewportAnchor,
};
use gpui_kit::{
    AnyWindowHandle, AsyncApp, Entity, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollEvent, ScrollSource, Size,
    TouchPhase, point, px, size,
};

use crate::cli::CliArgs;
use crate::fixtures::{
    DENSE_BLOCK_BYTES, DENSE_BLOCKS, DENSE_TOTAL_BYTES, SPARSE_LOGICAL_BYTES, ScratchLease,
    acquire_scratch_tempdir, close_and_verify_drained_scratch, create_dense_fixture,
    create_sparse_fixture, dense_disjoint_offset, disarm_scratch_on_unclean_abort,
    expected_dense_block_marker, expected_sparse_island_marker, mutate_dense_fixture_truncate,
    remove_fixture_file, sparse_island_anchor,
};
use crate::harness::{MultiviewPagedHarness, make_continuation_hold_gate};
use crate::model::PagedSourceState;
use crate::report::{
    CancelIsolationAudit, CloseLifecycleAudit, ExperimentReport, PerViewAudit,
    SearchOwnershipAudit, SourceChangedAudit, SparsePhaseAudit, resolve_report_target_canonical,
    write_and_flush_report,
};
pub const MIN_LEAF_WIDTH_PX: f32 = 240.0;
pub const MIN_LEAF_HEIGHT_PX: f32 = 160.0;

pub fn spawn_experiment_runner(
    cli: CliArgs,
    scratch_lease: ScratchLease,
    process_start: Instant,
    window: AnyWindowHandle,
    harness: Entity<MultiviewPagedHarness>,
    cx: &mut gpui_kit::App,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let mut report = ExperimentReport::new(cli.views);
        report.process_to_window_ms = process_start.elapsed().as_secs_f64() * 1000.0;

        // Validate & canonicalize `--report` off-thread before acquiring `TempDir` so invalid or
        // ambiguous paths (e.g. `/dev/null/fail.json` or missing `..` traversals) fail fast with
        // zero scratch directory creation.
        let raw_report_for_resolve = cli.report_path.clone();
        let resolved_report_res: Result<Option<PathBuf>, String> = match raw_report_for_resolve {
            Some(rp) => cx
                .background_executor()
                .spawn(async move { resolve_report_target_canonical(&rp).map(Some) })
                .await,
            None => Ok(None),
        };

        let mut report_file_blocked_reason: Option<String> = None;
        let mut resolved_report_target: Option<PathBuf> = None;

        let run_res = match resolved_report_res {
            Ok(resolved) => {
                resolved_report_target = resolved.clone();
                let mut report_aliased_scratch = false;
                let res = execute_experiment_phases(
                    &cli,
                    &scratch_lease,
                    resolved.as_deref(),
                    &mut report_aliased_scratch,
                    &window,
                    &harness,
                    &mut report,
                    cx,
                )
                .await;
                if report_aliased_scratch {
                    resolved_report_target = None;
                    report_file_blocked_reason =
                        Some("refusing to write --report inside leased TempDir".to_string());
                }
                res
            }
            Err(rp_err) => {
                report_file_blocked_reason = Some(rp_err.clone());
                Err(rp_err)
            }
        };

        if let Err(err) = &run_res {
            report.failure_reason = Some(err.clone());
        }

        // Teardown in-app entities & drain ALL background operations BEFORE closing TempDir.
        cx.update(|cx| {
            harness.update(cx, |h, cx| {
                h.teardown_all(cx);
            });
        });
        let drain_res =
            wait_background_ops_drained(&harness, true, cx, "shutdown background worker drain")
                .await;

        let total_frames = window
            .update(cx, |_, win, cx| {
                let frames = harness.read(cx).frames_rendered;
                win.refresh();
                win.draw(cx).clear(cx);
                win.remove_window();
                frames.saturating_add(1)
            })
            .unwrap_or(report.frames_drawn_total);
        report.frames_drawn_total = total_frames;

        match drain_res {
            Ok(()) => {
                let lease_for_close = scratch_lease.clone();
                let cleanup_res = cx
                    .background_executor()
                    .spawn(async move { close_and_verify_drained_scratch(&lease_for_close) })
                    .await;
                match cleanup_res {
                    Ok(Some(true)) => {
                        report.scratch_cleanup_verified = true;
                    }
                    Ok(Some(false)) => {
                        report.scratch_cleanup_verified = false;
                        report.failure_reason = Some(match report.failure_reason.take() {
                            Some(prev) => format!(
                                "{prev}; owned TempDir still present after TempDir::close"
                            ),
                            None => "owned TempDir still present after TempDir::close".to_string(),
                        });
                    }
                    Ok(None) => {
                        // Never acquired (e.g. failed before Phase 1); zero unlink calls performed.
                        report.scratch_cleanup_verified = false;
                    }
                    Err(cleanup_err) => {
                        report.scratch_cleanup_verified = false;
                        report.failure_reason = Some(match report.failure_reason.take() {
                            Some(prev) => format!("{prev}; cleanup error: {cleanup_err}"),
                            None => cleanup_err,
                        });
                    }
                }
            }
            Err(drain_err) => {
                // Guardrail 1: Workers did not finish draining; disarm `TempDir::drop` (`keep()`)
                // rather than running `remove_dir_all` concurrently with live workers.
                let residue = disarm_scratch_on_unclean_abort(&scratch_lease);
                report.scratch_cleanup_verified = false;
                let drain_msg = match residue {
                    Some(res_path) => format!(
                        "{drain_err}; disarmed TempDir cleanup while workers remained live, residue retained at '{}'",
                        res_path.display()
                    ),
                    None => drain_err,
                };
                report.failure_reason = Some(match report.failure_reason.take() {
                    Some(prev) => format!("{prev}; {drain_msg}"),
                    None => drain_msg,
                });
            }
        }

        report.total_elapsed_ms = process_start.elapsed().as_secs_f64() * 1000.0;
        report.passed =
            run_res.is_ok() && report_file_blocked_reason.is_none() && report.all_gates_satisfied();
        if !report.passed && report.failure_reason.is_none() {
            report.failure_reason =
                Some("one or more behavioral acceptance gates were not satisfied".to_string());
        }

        // Write report & flush stdio on background worker BEFORE `cx.quit()`.
        let report_for_io = report.clone();
        let blocked_msg = report_file_blocked_reason.clone();
        let write_res = cx
            .background_executor()
            .spawn(async move {
                let res =
                    write_and_flush_report(&report_for_io, resolved_report_target.as_deref());
                if let Some(blocked) = blocked_msg {
                    eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {blocked}");
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    return Err(blocked);
                }
                res
            })
            .await;
        if write_res.is_err() {
            std::process::exit(1);
        }

        if report.passed {
            cx.update(|cx| cx.quit());
        } else {
            std::process::exit(1);
        }
    })
    .detach();
}

async fn execute_experiment_phases(
    cli: &CliArgs,
    scratch_lease: &ScratchLease,
    resolved_report_path: Option<&Path>,
    report_aliased_scratch: &mut bool,
    window: &AnyWindowHandle,
    harness: &Entity<MultiviewPagedHarness>,
    report: &mut ExperimentReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let n = cli.views;
    // -------------------------------------------------------------------------
    // Phase 0: Ample Native Window Setup & Initial Drawn Frame
    // -------------------------------------------------------------------------
    let desired_window_size = size(px(1440.0), px(860.0));
    window
        .update(cx, |_, win, _| {
            win.resize(desired_window_size);
        })
        .map_err(|e| format!("window.resize failed: {e}"))?;
    pump_settle_frames(window, cx, 3).await?;

    let (vp_size, scale_factor, is_active, frames_now) = window
        .update(cx, |_, win, cx| {
            (
                win.viewport_size(),
                win.scale_factor(),
                win.is_window_active(),
                harness.read(cx).frames_rendered,
            )
        })
        .map_err(|e| format!("window metrics read failed: {e}"))?;

    report.window_viewport_width_px = vp_size.width.as_f32();
    report.window_viewport_height_px = vp_size.height.as_f32();
    report.window_scale_factor = scale_factor;
    report.window_is_active = is_active;
    report.native_frame_drawn = frames_now > 0;
    report.frames_drawn_total = frames_now;

    if vp_size.width.as_f32() < 960.0 || vp_size.height.as_f32() < 540.0 {
        return Err(format!(
            "window viewport ({:.1}x{:.1}) is too small to host ample >=240x160 multiview grid",
            vp_size.width.as_f32(),
            vp_size.height.as_f32()
        ));
    }

    // -------------------------------------------------------------------------
    // Phase 1: Atomic TempDir Acquisition & Dense Bounded Fixture Creation (off-thread)
    // -------------------------------------------------------------------------
    let lease_for_acquire = scratch_lease.clone();
    let (scratch_dir_path, canonical_scratch_dir) = cx
        .background_executor()
        .spawn(async move { acquire_scratch_tempdir(&lease_for_acquire) })
        .await?;

    if let Some(resolved_rp) = resolved_report_path
        && (resolved_rp == canonical_scratch_dir.as_path()
            || resolved_rp.starts_with(&canonical_scratch_dir))
    {
        *report_aliased_scratch = true;
        return Err(format!(
            "--report path resolves inside leased TempDir '{}'",
            canonical_scratch_dir.display()
        ));
    }

    let scratch_for_dense = scratch_dir_path.clone();
    let dense_spec = cx
        .background_executor()
        .spawn(async move { create_dense_fixture(&scratch_for_dense) })
        .await?;
    report.dense_fixture.expected_lines = dense_spec.expected_index_total_lines;
    report.dense_fixture.expected_search_matches = DENSE_BLOCKS;

    let paging_service = cx.update(|cx| harness.read(cx).paging.clone());
    let pre_open_stats = paging_service.stats();
    if pre_open_stats.sources != 0 || pre_open_stats.pages != 0 {
        return Err(format!(
            "expected clean PagingService before dense open, got sources={} pages={}",
            pre_open_stats.sources, pre_open_stats.pages
        ));
    }

    // -------------------------------------------------------------------------
    // Phase 2: Single Shared Source Open & N-View Native Attach + First Draw
    // -------------------------------------------------------------------------
    let t_open = Instant::now();
    let dense_source = open_paged_source_off_thread(
        paging_service.clone(),
        DocumentId::new(),
        LifecycleGen(1),
        "dense-paged-fixture.txt".to_string(),
        dense_spec.path.clone(),
        1024 * 1024, // > 1 MiB routes 8 MiB dense fixture through real OpenOutcome::Paged
        cx,
    )
    .await?;
    report.dense_source_open_ms = t_open.elapsed().as_secs_f64() * 1000.0;

    let t_attach = Instant::now();
    let initial_zero_anchors = vec![0u64; n];
    let view_ids = window
        .update(cx, |_, win, cx| -> Result<Vec<_>, String> {
            let ids = harness.update(cx, |h, cx| {
                h.install_source(dense_source, cx);
                h.attach_views(&initial_zero_anchors, cx)
            })?;
            win.refresh();
            win.draw(cx).clear(cx);
            Ok(ids)
        })
        .map_err(|e| format!("attach_views window update failed: {e}"))??;
    report.attach_n_views_and_first_draw_ms = t_attach.elapsed().as_secs_f64() * 1000.0;

    let stats_after_attach = paging_service.stats();
    if stats_after_attach.sources != 1 || stats_after_attach.peak_open_jobs > 1 {
        return Err(format!(
            "single source authority violated on attach: sources={} peak_open_jobs={}",
            stats_after_attach.sources, stats_after_attach.peak_open_jobs
        ));
    }

    // -------------------------------------------------------------------------
    // Phase 3: Same-Offset (0..65536) Hydration & Page-Cache Deduplication
    // -------------------------------------------------------------------------
    let t_same = Instant::now();
    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            for &vid in &view_ids {
                h.request_viewport(vid, ViewportAnchor::ByteOffset(0), false, cx)?;
            }
            Ok(())
        })
    })?;
    wait_views_settled(window, harness, cx, "same-offset hydration").await?;
    draw_frame(window, cx)?;
    report.same_offset_hydration_and_draw_ms = t_same.elapsed().as_secs_f64() * 1000.0;

    // Outside `t_same`: for N == 1, issue a second read at ByteOffset(0) so repeated-read page
    // deduplication is still behaviorally exercised without skewing 1-view hydration timing.
    if n == 1 {
        cx.update(|cx| -> Result<(), String> {
            harness.update(cx, |h, cx| {
                h.request_viewport(view_ids[0], ViewportAnchor::ByteOffset(0), false, cx)?;
                Ok(())
            })
        })?;
        wait_views_settled(window, harness, cx, "single-view repeat offset-0 dedup").await?;
        draw_frame(window, cx)?;
    }
    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let snap = h.snapshot_resources("dense_same_offset");
        let Some(src) = &h.source else {
            return Err("missing source after same-offset hydration".to_string());
        };
        if snap.paging_sources != 1 || snap.paging_pages != 1 || snap.paging_cached_bytes != 65_536
        {
            return Err(format!(
                "same-offset cache deduplication failed for N={n}: sources={} pages={} cached_bytes={}",
                snap.paging_sources, snap.paging_pages, snap.paging_cached_bytes
            ));
        }
        let expected_marker = expected_dense_block_marker(0);
        for v in &h.views {
            let Some(slice) = &v.current_slice else {
                return Err("view missing slice after same-offset settle".to_string());
            };
            if slice.source_gen != src.source_gen
                || slice.byte_range != (0..65_536)
                || !slice.text.starts_with(&expected_marker)
                || v.continuation_segment_count() < 2
            {
                return Err(format!(
                    "same-offset slice content/continuation mismatch on view slot {}",
                    v.slot_index
                ));
            }
        }
        report.same_offset_stats = snap;
        report.single_source_authority_verified = true;
        report.same_offset_cache_dedup_verified = true;
        Ok(())
    })?;

    // -------------------------------------------------------------------------
    // Phase 4: Disjoint Offsets Within Shared Cache Budget & Steady Frame Draw
    // -------------------------------------------------------------------------
    let t_disjoint = Instant::now();
    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            if n == 1 {
                h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(1)),
                    false,
                    cx,
                )?;
            } else {
                for (slot, &vid) in view_ids.iter().enumerate() {
                    h.request_viewport(
                        vid,
                        ViewportAnchor::ByteOffset(dense_disjoint_offset(slot)),
                        false,
                        cx,
                    )?;
                }
            }
            Ok(())
        })
    })?;
    wait_views_settled(window, harness, cx, "disjoint-offset hydration").await?;
    draw_frame(window, cx)?;
    report.disjoint_offset_hydration_and_draw_ms = t_disjoint.elapsed().as_secs_f64() * 1000.0;

    // Measure a settled steady-state synchronous native frame draw across all N hydrated views.
    let t_steady = Instant::now();
    draw_frame(window, cx)?;
    report.disjoint_steady_frame_draw_ms = t_steady.elapsed().as_secs_f64() * 1000.0;

    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let snap = h.snapshot_resources("dense_disjoint_offsets");
        let expected_pages = if n == 1 { 2 } else { n };
        if snap.paging_sources != 1
            || snap.paging_pages != expected_pages
            || snap.paging_cached_bytes != expected_pages * 65_536
            || snap.paging_cached_bytes > snap.paging_cache_budget
        {
            return Err(format!(
                "disjoint-offset cache accounting mismatch for N={n}: sources={} pages={} (expected {expected_pages}) cached_bytes={} budget={}",
                snap.paging_sources,
                snap.paging_pages,
                snap.paging_cached_bytes,
                snap.paging_cache_budget
            ));
        }

        for (slot, v) in h.views.iter().enumerate() {
            let target_blk = if n == 1 { 1 } else { slot };
            let expected_start = dense_disjoint_offset(target_blk);
            let expected_marker = expected_dense_block_marker(target_blk);
            let Some(slice) = &v.current_slice else {
                return Err(format!("missing disjoint slice in view slot {slot}"));
            };
            let vs = v.capture_view_state();
            if slice.byte_range != (expected_start..(expected_start + 65_536))
                || vs.cursor_byte != expected_start
                || vs.scroll_byte != expected_start
                || !slice.text.starts_with(&expected_marker)
                || v.continuation_segment_count() < 2
            {
                return Err(format!(
                    "disjoint slice verification failed in slot {slot}: range={:?} view_state={:?}",
                    slice.byte_range, vs
                ));
            }
        }
        report.disjoint_offset_stats = snap;
        report.disjoint_offsets_within_budget_verified = true;
        Ok(())
    })?;

    // -------------------------------------------------------------------------
    // Phase 5: Cooperative Background Sparse Indexing & ApproxLine Verification
    // -------------------------------------------------------------------------
    let t_index = Instant::now();
    cx.update(|cx| {
        harness.update(cx, |h, cx| {
            h.ensure_cooperative_indexer_running(cx);
        });
    });
    wait_until(
        Duration::from_secs(15),
        window,
        cx,
        |cx| {
            cx.update(|cx| {
                let h = harness.read(cx);
                h.source.as_ref().is_some_and(|s| {
                    s.source_error.is_some()
                        || matches!(s.index_status, IndexStatus::Complete { .. })
                })
            })
        },
        "cooperative background index_step completion",
    )
    .await?;
    draw_frame(window, cx)?;
    report.cooperative_full_index_ms = t_index.elapsed().as_secs_f64() * 1000.0;

    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let Some(src) = &h.source else {
            return Err("missing source after indexing".to_string());
        };
        if let Some(err) = &src.source_error {
            return Err(format!("cooperative indexing failed with error: {err}"));
        }
        let IndexStatus::Complete {
            total_bytes,
            total_lines,
        } = src.index_status
        else {
            return Err(format!(
                "expected IndexStatus::Complete, got {:?}",
                src.index_status
            ));
        };
        if total_bytes != DENSE_TOTAL_BYTES || total_lines != dense_spec.expected_index_total_lines
        {
            return Err(format!(
                "index totals mismatch: got bytes={total_bytes} lines={total_lines}, expected bytes={} lines={}",
                DENSE_TOTAL_BYTES, dense_spec.expected_index_total_lines
            ));
        }
        let snap = h.snapshot_resources("dense_post_index");
        if snap.paging_sources != 1
            || snap.peak_index_jobs > 1
            || snap.paging_index_bytes > snap.paging_index_budget
            || src.handle.index_checkpoint_count() == 0
        {
            return Err(format!(
                "post-index resource/checkpoint invariant failed: sources={} peak_index_jobs={} checkpoints={}",
                snap.paging_sources,
                snap.peak_index_jobs,
                src.handle.index_checkpoint_count()
            ));
        }
        report.post_index_stats = snap;
        Ok(())
    })?;

    // Exercise ApproxLine(16_377) -> exact start of Block 1 (1_048_576), then restore View 0.
    let block1_first_line_0based: u64 = 16_377;
    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            h.request_viewport(
                view_ids[0],
                ViewportAnchor::ApproxLine(block1_first_line_0based),
                false,
                cx,
            )?;
            Ok(())
        })
    })?;
    wait_views_settled(window, harness, cx, "ApproxLine probe").await?;
    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let Some(slice) = &h.views[0].current_slice else {
            return Err("missing slice after ApproxLine probe".to_string());
        };
        if slice.byte_range.start != DENSE_BLOCK_BYTES
            || slice.first_line_number != Some(block1_first_line_0based)
        {
            return Err(format!(
                "ApproxLine({block1_first_line_0based}) mismatch: start={} first_line_number={:?}",
                slice.byte_range.start, slice.first_line_number
            ));
        }
        Ok(())
    })?;
    if n >= 2 {
        cx.update(|cx| -> Result<(), String> {
            harness.update(cx, |h, cx| {
                h.request_viewport(view_ids[0], ViewportAnchor::ByteOffset(0), false, cx)?;
                Ok(())
            })
        })?;
        wait_views_settled(window, harness, cx, "restore view 0 to offset 0").await?;
    }
    report.cooperative_index_complete_verified = true;

    // -------------------------------------------------------------------------
    // Phase 6: Measured Bounds (>=240x160), Virtualization Culling & Native Wheel Scroll
    // -------------------------------------------------------------------------
    pump_settle_frames(window, cx, 2).await?;

    let (wheel_target_center, pre_wheel_audit) = cx.update(|cx| -> Result<_, String> {
        let h = harness.read(cx);
        let leaf_map = h.measured_leaf_bounds.borrow();
        let vis_map = h.visible_ranges_log.borrow();

        if leaf_map.len() != n {
            return Err(format!(
                "expected {n} prepainted leaf bounds, observed {}",
                leaf_map.len()
            ));
        }

        // Verify pairwise non-overlapping leaf bounds.
        for i in 0..leaf_map.len() {
            for j in (i + 1)..leaf_map.len() {
                if bounds_intersect_strict(leaf_map[i].1, leaf_map[j].1) {
                    return Err(format!(
                        "leaf bounds overlap between view {i} ({:?}) and view {j} ({:?})",
                        leaf_map[i].1, leaf_map[j].1
                    ));
                }
            }
        }

        let mut min_lw = f32::INFINITY;
        let mut min_lh = f32::INFINITY;
        let mut min_vw = f32::INFINITY;
        let mut min_vh = f32::INFINITY;
        let mut audits = Vec::with_capacity(n);
        let mut target_center = point(px(0.0), px(0.0));

        for (slot, v) in h.views.iter().enumerate() {
            let Some((_, leaf_b)) = leaf_map.iter().find(|(id, _)| *id == v.view_id).copied()
            else {
                return Err(format!("missing leaf bounds for slot {slot}"));
            };
            let vlist_b = v.scroll_handle.base_handle().bounds();
            let lw = leaf_b.size.width.as_f32();
            let lh = leaf_b.size.height.as_f32();
            let vw = vlist_b.size.width.as_f32();
            let vh = vlist_b.size.height.as_f32();
            min_lw = min_lw.min(lw);
            min_lh = min_lh.min(lh);
            min_vw = min_vw.min(vw);
            min_vh = min_vh.min(vh);

            if lw < MIN_LEAF_WIDTH_PX
                || lh < MIN_LEAF_HEIGHT_PX
                || vw < MIN_LEAF_WIDTH_PX
                || vh < MIN_LEAF_HEIGHT_PX
            {
                return Err(format!(
                    "bounds below 240x160 minimum in slot {slot} (N={n}): leaf={lw:.1}x{lh:.1} vlist={vw:.1}x{vh:.1}"
                ));
            }

            let Some(vis_range) = vis_map.get(&v.view_id).cloned() else {
                return Err(format!("missing v_virtual_list visible_range for slot {slot}"));
            };
            let seg_len = v.cached_segments.len();
            let item_len = v.segment_item_sizes.len();
            let culled = !vis_range.is_empty() && vis_range.len() * 4 < seg_len && seg_len == item_len;
            if !culled || vis_range.start != 0 {
                return Err(format!(
                    "virtualization culling invariant failed before wheel in slot {slot}: vis={vis_range:?} seg_len={seg_len} item_len={item_len}"
                ));
            }

            let offset_before = v.scroll_handle.base_handle().offset().y.as_f32();
            if offset_before.abs() > 0.5 {
                return Err(format!(
                    "expected initial scroll offset 0 in slot {slot}, got {offset_before}"
                ));
            }

            if slot == n - 1 {
                target_center = vlist_b.center();
            }

            let range = v.visible_byte_range();
            let vs = v.capture_view_state();
            audits.push(PerViewAudit {
                slot_index: slot,
                view_id: v.view_id.key(),
                leaf_bounds_xywh_px: [
                    leaf_b.origin.x.as_f32(),
                    leaf_b.origin.y.as_f32(),
                    lw,
                    lh,
                ],
                vlist_bounds_xywh_px: [
                    vlist_b.origin.x.as_f32(),
                    vlist_b.origin.y.as_f32(),
                    vw,
                    vh,
                ],
                slice_byte_range: [range.start, range.end],
                captured_cursor_byte: vs.cursor_byte,
                captured_scroll_byte: vs.scroll_byte,
                cached_segment_count: seg_len,
                item_size_count: item_len,
                continuation_segment_count: v.continuation_segment_count(),
                visible_row_range_before_wheel: [vis_range.start, vis_range.end],
                culled_properly: culled,
                scroll_offset_y_before_wheel_px: offset_before,
                scroll_offset_y_after_wheel_px: offset_before,
                visible_row_range_after_wheel: [vis_range.start, vis_range.end],
            });
        }

        report.min_leaf_width_px = min_lw;
        report.min_leaf_height_px = min_lh;
        report.min_vlist_width_px = min_vw;
        report.min_vlist_height_px = min_vh;
        report.all_views_min_bounds_240x160_verified = true;
        report.segment_virtualization_culling_verified = true;
        Ok((target_center, audits))
    })?;

    // Dispatch actual native PlatformInput::Scroll wheel event at the target view's measured center.
    let t_wheel = Instant::now();
    window
        .update(cx, |_, win, cx| {
            win.dispatch_event(
                PlatformInput::Scroll(ScrollEvent {
                    position: wheel_target_center,
                    delta: ScrollDelta::Pixels(point(px(0.0), px(-260.0))),
                    modifiers: Modifiers::default(),
                    touch_phase: TouchPhase::Started,
                    source: ScrollSource::Pointer,
                }),
                cx,
            );
            win.refresh();
            win.draw(cx).clear(cx);
        })
        .map_err(|e| format!("native PlatformInput::Scroll dispatch failed: {e}"))?;
    report.native_wheel_scroll_and_draw_ms = t_wheel.elapsed().as_secs_f64() * 1000.0;

    let mut finalized_audits = pre_wheel_audit;
    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let vis_map = h.visible_ranges_log.borrow();

        for (slot, v) in h.views.iter().enumerate() {
            let off_after = v.scroll_handle.base_handle().offset().y.as_f32();
            let Some(vis_after) = vis_map.get(&v.view_id).cloned() else {
                return Err(format!("missing post-wheel visible_range for slot {slot}"));
            };
            finalized_audits[slot].scroll_offset_y_after_wheel_px = off_after;
            finalized_audits[slot].visible_row_range_after_wheel =
                [vis_after.start, vis_after.end];

            if slot == n - 1 {
                if off_after > -200.0
                    || vis_after.start < 10
                    || vis_after.len() >= v.cached_segments.len()
                {
                    return Err(format!(
                        "wheel target slot {slot} did not scroll/cull as expected: offset_y={off_after} vis_after={vis_after:?}"
                    ));
                }
            } else if off_after.abs() > 0.5 || vis_after.start != 0 {
                return Err(format!(
                    "non-target slot {slot} scrolled unexpectedly when wheel targeted slot {}: offset_y={off_after} vis_after={vis_after:?}",
                    n - 1
                ));
            }
        }
        Ok(())
    })?;
    report.per_view_disjoint_audit = finalized_audits;
    report.native_wheel_independent_scroll_verified = true;

    // -------------------------------------------------------------------------
    // Phase 7: Streaming SearchService & Focus Highlight Ownership
    // -------------------------------------------------------------------------
    let view0_center = Point {
        x: px(report.per_view_disjoint_audit[0].vlist_bounds_xywh_px[0]
            + report.per_view_disjoint_audit[0].vlist_bounds_xywh_px[2] * 0.5),
        y: px(report.per_view_disjoint_audit[0].vlist_bounds_xywh_px[1]
            + report.per_view_disjoint_audit[0].vlist_bounds_xywh_px[3] * 0.5),
    };
    click_point(window, cx, view0_center)?;

    let (search_svc, search_target) = cx.update(|cx| -> Result<_, String> {
        let h = harness.read(cx);
        if h.active_view != Some(view_ids[0]) {
            return Err("native click did not focus view 0".to_string());
        }
        let Some(src) = &h.source else {
            return Err("missing source before search".to_string());
        };
        Ok((
            h.search.clone(),
            SearchTarget::PagedFile {
                handle: src.handle.clone(),
                source_gen: src.source_gen,
                encoding: src.meta.encoding,
            },
        ))
    })?;

    let search_req = SearchRequest::new(
        QueryId(1),
        dense_spec.search_needle.as_bytes().to_vec(),
        search_target,
    );
    let search_task = search_svc
        .start(search_req)
        .map_err(|e| format!("SearchService::start failed: {e:?}"))?;
    let active_slots_sampled = search_svc.active_tasks();
    if active_slots_sampled > 1 {
        return Err(format!(
            "SearchService exceeded single-document slot budget: active_slots_sampled={active_slots_sampled}"
        ));
    }

    let (matches, bytes_scanned) = drain_search_task_off_thread(search_task, cx).await?;
    if matches.len() != DENSE_BLOCKS || bytes_scanned != DENSE_TOTAL_BYTES {
        return Err(format!(
            "streaming search mismatch: matches={} (expected {DENSE_BLOCKS}) bytes_scanned={bytes_scanned}",
            matches.len()
        ));
    }
    for (blk, m) in matches.iter().enumerate() {
        if m.byte_range.start != dense_spec.expected_match_offsets[blk] {
            return Err(format!(
                "search match #{blk} offset mismatch: got {}, expected {}",
                m.byte_range.start, dense_spec.expected_match_offsets[blk]
            ));
        }
    }

    // Highlight match in View 0's current slice (Block 0 if N>=2, Block 1 if N==1) and draw frame.
    let v0_match_ix = if n == 1 { 1 } else { 0 };
    let v0_match_range = matches[v0_match_ix].byte_range.clone();
    cx.update(|cx| {
        harness.update(cx, |h, cx| {
            // Ensure row 2 is visible at the top of View 0 (in case N==1 scrolled View 0 in Phase 6).
            h.views[0]
                .scroll_handle
                .scroll_to_item(0, gpui_kit::ScrollStrategy::Top);
            h.set_active_view_highlight(Some(v0_match_range.clone()), cx);
        });
    });
    draw_frame(window, cx)?;

    let initial_highlighted_views = cx.update(|cx| -> Result<Vec<String>, String> {
        let h = harness.read(cx);
        let painted = h.rendered_highlight_views.borrow();
        if painted.is_empty() || painted.iter().any(|(vid, _)| *vid != view_ids[0]) {
            return Err(format!(
                "expected painted highlight exclusively on view 0, observed {painted:?}"
            ));
        }
        for (ix, v) in h.views.iter().enumerate() {
            if ix == 0 {
                if v.highlighted_byte_range != Some(v0_match_range.clone()) {
                    return Err("view 0 missing model highlight".to_string());
                }
            } else if v.highlighted_byte_range.is_some() {
                return Err(format!("non-active view {ix} gained search highlight"));
            }
        }
        Ok(vec![view_ids[0].key()])
    })?;

    let mut search_audit = SearchOwnershipAudit {
        active_slots_sampled,
        matches_emitted: matches.len(),
        bytes_scanned,
        initial_active_view_id: view_ids[0].key(),
        initial_highlighted_views,
        switched_active_view_id: None,
        prior_view_highlight_cleared_on_switch: None,
        switched_highlighted_views: Vec::new(),
        switched_anchor_byte: None,
        non_active_anchors_unchanged: true,
    };

    if n >= 2 {
        let prior_non_last_anchors: Vec<u64> = cx.update(|cx| {
            harness
                .read(cx)
                .views
                .iter()
                .take(n - 1)
                .map(|v| v.current_anchor_byte)
                .collect()
        });

        let last_audit = &report.per_view_disjoint_audit[n - 1];
        let last_center = Point {
            x: px(last_audit.vlist_bounds_xywh_px[0] + last_audit.vlist_bounds_xywh_px[2] * 0.5),
            y: px(last_audit.vlist_bounds_xywh_px[1] + last_audit.vlist_bounds_xywh_px[3] * 0.5),
        };
        click_point(window, cx, last_center)?;

        let jump_blk = if n == 8 { 2usize } else { 7usize };
        let jump_anchor = dense_disjoint_offset(jump_blk);
        let jump_match = matches[jump_blk].byte_range.clone();
        let prior_last_anchor = cx.update(|cx| harness.read(cx).views[n - 1].current_anchor_byte);
        if prior_last_anchor == jump_anchor {
            return Err(
                "internal test bug: switched search jump anchor must differ from prior anchor"
                    .to_string(),
            );
        }

        cx.update(|cx| -> Result<(), String> {
            harness.update(cx, |h, cx| {
                if h.active_view != Some(view_ids[n - 1]) {
                    return Err("native click did not switch active_view to last pane".to_string());
                }
                if h.views[0].highlighted_byte_range.is_some() {
                    return Err(
                        "switching active pane did not clear prior view's search highlight"
                            .to_string(),
                    );
                }
                h.request_viewport(
                    view_ids[n - 1],
                    ViewportAnchor::ByteOffset(jump_anchor),
                    false,
                    cx,
                )?;
                h.set_active_view_highlight(Some(jump_match.clone()), cx);
                Ok(())
            })
        })?;
        wait_views_settled(window, harness, cx, "search jump on switched active view").await?;
        draw_frame(window, cx)?;

        cx.update(|cx| -> Result<(), String> {
            let h = harness.read(cx);
            let painted = h.rendered_highlight_views.borrow();
            if painted.is_empty() || painted.iter().any(|(vid, _)| *vid != view_ids[n - 1]) {
                return Err(format!(
                    "expected painted highlight exclusively on switched view {}, observed {painted:?}",
                    n - 1
                ));
            }
            if h.views[n - 1].current_anchor_byte != jump_anchor {
                return Err("switched view did not jump to search match anchor".to_string());
            }
            for (ix, &expected_anchor) in prior_non_last_anchors.iter().enumerate() {
                if h.views[ix].current_anchor_byte != expected_anchor
                    || h.views[ix].highlighted_byte_range.is_some()
                {
                    return Err(format!(
                        "non-active view {ix} mutated during search jump on active view"
                    ));
                }
            }
            search_audit.switched_active_view_id = Some(view_ids[n - 1].key());
            search_audit.prior_view_highlight_cleared_on_switch = Some(true);
            search_audit.switched_highlighted_views = vec![view_ids[n - 1].key()];
            search_audit.switched_anchor_byte = Some(jump_anchor);
            search_audit.non_active_anchors_unchanged = true;
            Ok(())
        })?;
    }

    cx.update(|cx| {
        harness.update(cx, |h, cx| h.set_active_view_highlight(None, cx));
    });
    report.search_ownership_audit = search_audit;
    report.search_and_focus_highlight_ownership_verified = true;

    // -------------------------------------------------------------------------
    // Phase 8: Request Cancellation & Real Async req_seq Fence Isolation
    // -------------------------------------------------------------------------
    let (hold_gate_p8, entered_rx_p8, release_tx_p8, delivered_rx_p8) =
        make_continuation_hold_gate();
    let tok_held_p8 = cx.update(|cx| -> Result<CancelToken, String> {
        harness.update(cx, |h, cx| {
            let (_tag_held, tok_held) = h.request_viewport_with_gate(
                view_ids[0],
                ViewportAnchor::ByteOffset(dense_disjoint_offset(3)),
                false,
                Some(hold_gate_p8),
                cx,
            )?;
            Ok(tok_held)
        })
    })?;
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |_| entered_rx_p8.try_recv().is_ok(),
        "Phase 8 real background viewport completion before supersession",
    )
    .await?;

    if n >= 2 {
        let (tok_a1, tok_a2, tok_b) = cx.update(|cx| -> Result<_, String> {
            harness.update(cx, |h, cx| {
                let (_tag_a1, tok_a1) = h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(2)),
                    false,
                    cx,
                )?;
                let (_tag_b, tok_b) = h.request_viewport(
                    view_ids[1],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(5)),
                    false,
                    cx,
                )?;
                let (_tag_a2, tok_a2) = h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(4)),
                    false,
                    cx,
                )?;
                Ok((tok_a1, tok_a2, tok_b))
            })
        })?;

        if !tok_held_p8.is_cancelled()
            || !tok_a1.is_cancelled()
            || tok_a2.is_cancelled()
            || tok_b.is_cancelled()
        {
            let _ = release_tx_p8.send(());
            return Err(format!(
                "cancel isolation token state wrong: held_cancelled={} a1_cancelled={} a2_cancelled={} b_cancelled={}",
                tok_held_p8.is_cancelled(),
                tok_a1.is_cancelled(),
                tok_a2.is_cancelled(),
                tok_b.is_cancelled()
            ));
        }

        wait_views_settled(window, harness, cx, "concurrent cancel isolation").await?;

        // Release the REAL held 3 MiB async continuation after View 0 settled at 4 MiB.
        let _ = release_tx_p8.send(());
        wait_until(
            Duration::from_secs(5),
            window,
            cx,
            |_| delivered_rx_p8.try_recv().is_ok(),
            "Phase 8 held stale async completion delivery",
        )
        .await?;
        draw_frame(window, cx)?;

        cx.update(|cx| -> Result<(), String> {
            let h = harness.read(cx);
            let s0 = h.views[0]
                .current_slice
                .as_ref()
                .ok_or("view 0 missing slice after cancel test")?;
            let s1 = h.views[1]
                .current_slice
                .as_ref()
                .ok_or("view 1 missing slice after cancel test")?;
            if s0.byte_range.start != dense_disjoint_offset(4)
                || !s0.text.starts_with(&expected_dense_block_marker(4))
            {
                return Err(format!(
                    "view 0 did not stay on replacement offset 4 MiB after stale async delivery: got {}",
                    s0.byte_range.start
                ));
            }
            if s1.byte_range.start != dense_disjoint_offset(5)
                || !s1.text.starts_with(&expected_dense_block_marker(5))
            {
                return Err(format!(
                    "view 1 sibling request failed/cancelled unexpectedly: got {}",
                    s1.byte_range.start
                ));
            }
            if h.stale_req_seq_dropped == 0 {
                return Err("expected real async stale_req_seq_dropped >= 1".to_string());
            }
            report.cancel_isolation_audit = CancelIsolationAudit {
                multi_view_applicable: true,
                superseded_token_cancelled: true,
                replacement_token_completed: true,
                sibling_token_uncancelled: Some(true),
                sibling_request_completed: Some(true),
                stale_req_seq_completions_dropped: h.stale_req_seq_dropped,
            };
            Ok(())
        })?;
    } else {
        let (tok_a1, tok_a2) = cx.update(|cx| -> Result<_, String> {
            harness.update(cx, |h, cx| {
                let (_tag_a1, tok_a1) = h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(2)),
                    false,
                    cx,
                )?;
                let (_tag_a2, tok_a2) = h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(4)),
                    false,
                    cx,
                )?;
                Ok((tok_a1, tok_a2))
            })
        })?;
        if !tok_held_p8.is_cancelled() || !tok_a1.is_cancelled() || tok_a2.is_cancelled() {
            let _ = release_tx_p8.send(());
            return Err("single-view supersession cancel state wrong".to_string());
        }
        wait_views_settled(window, harness, cx, "single-view supersession").await?;
        let _ = release_tx_p8.send(());
        wait_until(
            Duration::from_secs(5),
            window,
            cx,
            |_| delivered_rx_p8.try_recv().is_ok(),
            "Phase 8 single-view held stale async completion delivery",
        )
        .await?;
        draw_frame(window, cx)?;
        cx.update(|cx| -> Result<(), String> {
            let h = harness.read(cx);
            let s0 = h.views[0]
                .current_slice
                .as_ref()
                .ok_or("view 0 missing slice")?;
            if s0.byte_range.start != dense_disjoint_offset(4) || h.stale_req_seq_dropped == 0 {
                return Err("single-view supersession/fence check failed".to_string());
            }
            report.cancel_isolation_audit = CancelIsolationAudit {
                multi_view_applicable: false,
                superseded_token_cancelled: true,
                replacement_token_completed: true,
                sibling_token_uncancelled: None,
                sibling_request_completed: None,
                stale_req_seq_completions_dropped: h.stale_req_seq_dropped,
            };
            Ok(())
        })?;
    }
    report.request_cancel_isolation_verified = true;
    // -------------------------------------------------------------------------
    // Phase 9: View Close Lifecycle (N -> 1 preserving source/index, 1 -> 0 releasing all)
    // -------------------------------------------------------------------------
    let mut close_audit = CloseLifecycleAudit {
        initial_views: n,
        close_to_one_applicable: n > 1,
        ..Default::default()
    };

    if n > 1 {
        // Arm in-flight tokens on views[1..N] right before closing them so `Drop for PagedViewportState`
        // proves per-view cancellation without cancelling `source.index_cancel`.
        let closed_tokens = cx.update(|cx| -> Result<Vec<CancelToken>, String> {
            harness.update(cx, |h, cx| {
                let mut toks = Vec::with_capacity(n - 1);
                for &vid in &view_ids[1..] {
                    let (_tag, tok) = h.request_viewport(
                        vid,
                        ViewportAnchor::ByteOffset(dense_disjoint_offset(2)),
                        false,
                        cx,
                    )?;
                    toks.push(tok);
                }
                for &vid in &view_ids[1..] {
                    if !h.close_view(vid, cx) {
                        return Err(format!("failed to close view {}", vid.key()));
                    }
                }
                Ok(toks)
            })
        })?;
        draw_frame(window, cx)?;

        if !closed_tokens.iter().all(CancelToken::is_cancelled) {
            return Err("closing views 1..N did not cancel all closed views' tokens".to_string());
        }

        // Verify source & completed SparseIndex remain intact while View 0 survives, and perform a
        // fresh viewport read on surviving View 0.
        cx.update(|cx| -> Result<(), String> {
            harness.update(cx, |h, cx| {
                if h.views.len() != 1 {
                    return Err(format!("expected 1 surviving view, got {}", h.views.len()));
                }
                let stats = h.paging.stats();
                let Some(src) = &h.source else {
                    return Err("source dropped prematurely while 1 view survived".to_string());
                };
                let index_complete = matches!(
                    src.index_status,
                    IndexStatus::Complete { total_lines, .. }
                        if total_lines == dense_spec.expected_index_total_lines
                );
                let checkpoints = src.handle.index_checkpoint_count();
                if stats.sources != 1
                    || src.index_cancel.is_cancelled()
                    || !index_complete
                    || checkpoints == 0
                {
                    return Err(format!(
                        "close {n}->1 failed to preserve shared source/index: sources={} index_cancelled={} complete={index_complete} checkpoints={checkpoints}",
                        stats.sources,
                        src.index_cancel.is_cancelled()
                    ));
                }
                close_audit.closed_views_count = n - 1;
                close_audit.closed_view_tokens_cancelled = true;
                close_audit.sources_at_one_surviving_view = Some(stats.sources);
                close_audit.index_cancel_cancelled_at_one_view = Some(false);
                close_audit.index_complete_preserved_at_one_view = Some(true);
                close_audit.index_checkpoints_at_one_view = Some(checkpoints);

                h.request_viewport(
                    view_ids[0],
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(6)),
                    false,
                    cx,
                )?;
                Ok(())
            })
        })?;
        wait_views_settled(
            window,
            harness,
            cx,
            "surviving view fresh read after close N->1",
        )
        .await?;
        draw_frame(window, cx)?;

        cx.update(|cx| -> Result<(), String> {
            let h = harness.read(cx);
            let s0 = h.views[0]
                .current_slice
                .as_ref()
                .ok_or("surviving view missing slice")?;
            if s0.byte_range.start != dense_disjoint_offset(6)
                || !s0.text.starts_with(&expected_dense_block_marker(6))
            {
                return Err("surviving view failed fresh read after sibling close".to_string());
            }
            close_audit.surviving_view_fresh_read_verified = Some(true);
            report.close_n_to_1_preserved_verified = Some(true);
            Ok(())
        })?;
    } else {
        close_audit.closed_views_count = 0;
        close_audit.closed_view_tokens_cancelled = true;
        report.close_n_to_1_preserved_verified = None;
    }

    // Now close 1 -> 0 and release PagedSourceState.
    let dropped_index_token =
        cx.update(|cx| harness.update(cx, |h, cx| h.close_source_and_all_views(cx)));
    wait_background_ops_drained(harness, true, cx, "dense close 1->0 worker drain").await?;
    draw_frame(window, cx)?;

    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let snap = h.snapshot_resources("dense_closed_to_zero");
        let index_cancelled = dropped_index_token
            .as_ref()
            .is_some_and(CancelToken::is_cancelled);
        if snap.mounted_views != 0
            || snap.paging_sources != 0
            || snap.paging_pages != 0
            || snap.paging_cached_bytes != 0
            || snap.paging_index_bytes != 0
            || !index_cancelled
        {
            return Err(format!(
                "close 1->0 did not release all resources: snap={snap:?} index_cancelled={index_cancelled}"
            ));
        }
        close_audit.sources_after_last_view_closed = snap.paging_sources;
        close_audit.pages_after_last_view_closed = snap.paging_pages;
        close_audit.cached_bytes_after_last_view_closed = snap.paging_cached_bytes;
        close_audit.index_bytes_after_last_view_closed = snap.paging_index_bytes;
        close_audit.index_cancel_cancelled_after_last_view_closed = index_cancelled;
        report.close_to_zero_dense_stats = snap;
        report.close_lifecycle_audit = close_audit;
        report.close_1_to_0_released_verified = true;
        Ok(())
    })?;

    // -------------------------------------------------------------------------
    // Phase 10: Separate Sparse >4 GiB (>u32::MAX) Marker Source Lifecycle
    //           (64-bit Offset / ViewState / Gutter Correctness ONLY — No Throughput Claim)
    // -------------------------------------------------------------------------
    let sources_before_sparse = paging_service.stats().sources;
    if sources_before_sparse != 0 {
        return Err(format!(
            "expected sources == 0 before sparse lifecycle, got {sources_before_sparse}"
        ));
    }

    report.sparse_fixture.logical_byte_len = SPARSE_LOGICAL_BYTES;
    report.sparse_fixture.min_high_offset_byte = sparse_island_anchor(1);

    let scratch_for_sparse = scratch_dir_path.clone();
    let sparse_spec = match cx
        .background_executor()
        .spawn(async move { create_sparse_fixture(&scratch_for_sparse) })
        .await
    {
        Ok(spec) => {
            report.sparse_fixture.sparse_set_len_supported = true;
            spec
        }
        Err(err) => {
            report.sparse_fixture.sparse_set_len_supported = false;
            report.sparse_fixture.sparse_error = Some(err.message().to_string());
            return Err(format!(
                "sparse >4GiB fixture creation failed: {}",
                err.message()
            ));
        }
    };

    let sparse_source = open_paged_source_off_thread(
        paging_service.clone(),
        DocumentId::new(),
        LifecycleGen(1),
        "sparse-64bit-markers.txt".to_string(),
        sparse_spec.path.clone(),
        64 * 1024 * 1024, // Default 64 MiB limit; >4 GiB routes to Paged reading only 64 KiB prefix
        cx,
    )
    .await?;

    let sparse_initial_anchors: Vec<u64> = (0..n).map(sparse_island_anchor).collect();
    let sparse_view_ids = window
        .update(cx, |_, win, cx| -> Result<Vec<_>, String> {
            let ids = harness.update(cx, |h, cx| {
                h.install_source(sparse_source, cx);
                h.attach_views(&sparse_initial_anchors, cx)
            })?;
            win.refresh();
            win.draw(cx).clear(cx);
            Ok(ids)
        })
        .map_err(|e| format!("sparse attach_views failed: {e}"))??;

    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            for (slot, &vid) in sparse_view_ids.iter().enumerate() {
                h.request_viewport(
                    vid,
                    ViewportAnchor::ByteOffset(sparse_island_anchor(slot)),
                    false, // Never full-index the >4 GiB sparse marker file
                    cx,
                )?;
            }
            Ok(())
        })
    })?;
    wait_views_settled(window, harness, cx, "sparse initial hydration").await?;
    draw_frame(window, cx)?;

    // When N == 1, verify origin 0 first, then navigate View 0 to >= (1u64 << 32) so 64-bit
    // >4 GiB offset rendering is exercised even in the 1-view run.
    if n == 1 {
        cx.update(|cx| -> Result<(), String> {
            let h = harness.read(cx);
            let s0 = h.views[0]
                .current_slice
                .as_ref()
                .ok_or("sparse view 0 missing origin slice")?;
            if s0.byte_range.start != 0 || !s0.text.starts_with(&expected_sparse_island_marker(0)) {
                return Err("sparse view 0 origin marker check failed".to_string());
            }
            Ok(())
        })?;
        cx.update(|cx| -> Result<(), String> {
            harness.update(cx, |h, cx| {
                h.request_viewport(
                    sparse_view_ids[0],
                    ViewportAnchor::ByteOffset(sparse_island_anchor(1)),
                    false,
                    cx,
                )?;
                Ok(())
            })
        })?;
        wait_views_settled(window, harness, cx, "sparse single-view >4GiB jump").await?;
        draw_frame(window, cx)?;
    }

    let sources_while_sparse_open = paging_service.stats().sources;
    if sources_while_sparse_open != 1 {
        return Err(format!(
            "expected sources == 1 during sparse lifecycle, got {sources_while_sparse_open}"
        ));
    }

    let (view_anchors_64, scroll_bytes_64, gutter_samples) =
        cx.update(|cx| -> Result<_, String> {
            let h = harness.read(cx);
            let leaf_map = h.measured_leaf_bounds.borrow();
            let gutters = h.rendered_gutter_samples.borrow();

            if leaf_map.len() != n {
                return Err("sparse frame missing prepainted leaf bounds".to_string());
            }
            for (_, b) in leaf_map.iter() {
                if b.size.width.as_f32() < MIN_LEAF_WIDTH_PX
                    || b.size.height.as_f32() < MIN_LEAF_HEIGHT_PX
                {
                    return Err("sparse leaf bounds below 240x160".to_string());
                }
            }

            let mut anchors = Vec::with_capacity(n);
            let mut scrolls = Vec::with_capacity(n);
            let mut gutter_out = Vec::with_capacity(n);
            let mut saw_gt_u32 = false;

            for (slot, v) in h.views.iter().enumerate() {
                let expected_island = if n == 1 { 1 } else { slot };
                let expected_anchor = sparse_island_anchor(expected_island);
                let expected_marker = expected_sparse_island_marker(expected_island);
                let s = v
                    .current_slice
                    .as_ref()
                    .ok_or("missing slice in sparse verification")?;
                let vs = v.capture_view_state();
                let g_label = gutters
                    .get(&v.view_id)
                    .cloned()
                    .ok_or("missing rendered gutter sample in sparse verification")?;

                if s.lossy
                    || s.byte_range.start != expected_anchor
                    || vs.cursor_byte != expected_anchor
                    || vs.scroll_byte != expected_anchor
                    || !s.text.starts_with(&expected_marker)
                    || v.continuation_segment_count() < 2
                {
                    return Err(format!(
                        "sparse 64-bit verification failed in slot {slot}: range={:?} vs={vs:?} lossy={}",
                        s.byte_range, s.lossy
                    ));
                }

                if expected_island >= 1 {
                    if s.byte_range.start <= u32::MAX as u64
                        || vs.scroll_byte <= u32::MAX as u64
                        || !g_label.ends_with(&format!("@0x{expected_anchor:08X}"))
                    {
                        return Err(format!(
                            "sparse >4GiB u32-truncation check failed in slot {slot}: start=0x{:X} gutter='{g_label}'",
                            s.byte_range.start
                        ));
                    }
                    let cont_gt_u32 = v.cached_segments.iter().any(|seg| {
                        seg.is_continuation
                            && seg
                                .exact_file_byte_range
                                .as_ref()
                                .is_some_and(|r| r.start > u32::MAX as u64)
                    });
                    if !cont_gt_u32 {
                        return Err(format!(
                            "sparse >4GiB continuation segment missing >u32::MAX exact byte range in slot {slot}"
                        ));
                    }
                    saw_gt_u32 = true;
                }

                anchors.push(s.byte_range.start);
                scrolls.push(vs.scroll_byte);
                gutter_out.push(g_label);
            }

            if !saw_gt_u32 {
                return Err("sparse phase did not observe any >u32::MAX slice".to_string());
            }
            Ok((anchors, scrolls, gutter_out))
        })?;

    // Close Sparse lifecycle cleanly before reopening Dense for SourceChanged invalidation.
    cx.update(|cx| {
        harness.update(cx, |h, cx| {
            let _ = h.close_source_and_all_views(cx);
        });
    });
    wait_background_ops_drained(harness, true, cx, "sparse close worker drain").await?;
    draw_frame(window, cx)?;

    let sparse_path_to_rm = sparse_spec.path.clone();
    cx.background_executor()
        .spawn(async move { remove_fixture_file(&sparse_path_to_rm) })
        .await?;

    let stats_after_sparse = paging_service.stats();
    if stats_after_sparse.sources != 0
        || stats_after_sparse.pages != 0
        || stats_after_sparse.cached_bytes != 0
    {
        return Err(format!(
            "sparse lifecycle did not release cleanly: stats={stats_after_sparse:?}"
        ));
    }

    report.sparse_64bit_audit = SparsePhaseAudit {
        sources_before_open: sources_before_sparse,
        sources_while_open: sources_while_sparse_open,
        mounted_views: n,
        view_anchors_64bit: view_anchors_64,
        captured_scroll_bytes_64bit: scroll_bytes_64,
        rendered_gutter_samples: gutter_samples,
        continuation_segments_verified: true,
        offset_gt_u32_max_verified: true,
        frame_drawn: true,
        sources_after_close: stats_after_sparse.sources,
        pages_after_close: stats_after_sparse.pages,
        cached_bytes_after_close: stats_after_sparse.cached_bytes,
    };
    report.sparse_64bit_offset_verified = true;

    // -------------------------------------------------------------------------
    // Phase 11: All-View SourceChanged Invalidation & source_gen Completion Fence
    // -------------------------------------------------------------------------
    let dense_reopen = open_paged_source_off_thread(
        paging_service.clone(),
        DocumentId::new(),
        LifecycleGen(2),
        "dense-paged-fixture.txt".to_string(),
        dense_spec.path.clone(),
        1024 * 1024,
        cx,
    )
    .await?;

    let inv_anchors: Vec<u64> = (0..n).map(dense_disjoint_offset).collect();
    let inv_view_ids = window
        .update(cx, |_, win, cx| -> Result<Vec<_>, String> {
            let ids = harness.update(cx, |h, cx| {
                h.install_source(dense_reopen, cx);
                h.attach_views(&inv_anchors, cx)
            })?;
            win.refresh();
            win.draw(cx).clear(cx);
            Ok(ids)
        })
        .map_err(|e| format!("dense reopen attach failed: {e}"))??;

    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            for (slot, &vid) in inv_view_ids.iter().enumerate() {
                h.request_viewport(
                    vid,
                    ViewportAnchor::ByteOffset(dense_disjoint_offset(slot)),
                    true,
                    cx,
                )?;
            }
            Ok(())
        })
    })?;
    wait_views_settled(window, harness, cx, "pre-invalidation hydration").await?;
    draw_frame(window, cx)?;

    let sources_before_mut = cx.update(|cx| harness.read(cx).paging.stats().sources);
    if sources_before_mut != 1 {
        return Err(format!(
            "expected sources == 1 before disk mutation, got {sources_before_mut}"
        ));
    }

    // Hold a REAL pre-mutation background-completed `Ok(ViewportSlice)` continuation (on View 1
    // when N>=2 so its per-view `req_seq` remains current, or View 0 when N==1).
    let held_inv_vid = if n >= 2 {
        inv_view_ids[1]
    } else {
        inv_view_ids[0]
    };
    let (hold_pre_mut, entered_pre_mut, release_pre_mut, delivered_pre_mut) =
        make_continuation_hold_gate();
    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            h.request_viewport_with_gate(
                held_inv_vid,
                ViewportAnchor::ByteOffset(dense_disjoint_offset(1)),
                false,
                Some(hold_pre_mut),
                cx,
            )?;
            Ok(())
        })
    })?;
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |_| entered_pre_mut.try_recv().is_ok(),
        "pre-mutation real background viewport read completion",
    )
    .await?;

    // Truncate dense fixture on disk via background worker to trip real FileFingerprint mismatch.
    let dense_path_mut = dense_spec.path.clone();
    cx.background_executor()
        .spawn(async move { mutate_dense_fixture_truncate(&dense_path_mut) })
        .await?;

    // Trigger invalidating read on View 0 with a second hold gate: its worker detects truncation
    // and runs `PagedDocHandle::trip_invalidation` (bumping atomic `handle.source_generation()`)
    // BEFORE foreground `source.source_gen` or `source.source_error` is updated.
    let (hold_inv_read, entered_inv_read, release_inv_read, delivered_inv_read) =
        make_continuation_hold_gate();
    cx.update(|cx| -> Result<(), String> {
        harness.update(cx, |h, cx| {
            h.request_viewport_with_gate(
                inv_view_ids[0],
                ViewportAnchor::ByteOffset(4096),
                false,
                Some(hold_inv_read),
                cx,
            )?;
            Ok(())
        })
    })?;
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |_| entered_inv_read.try_recv().is_ok(),
        "post-truncation worker invalidation completion",
    )
    .await?;

    // Release the older `Ok(ViewportSlice)` continuation FIRST: `complete_viewport_fenced` must
    // catch the bumped live `handle.source_generation()`, invalidate all views, and increment
    // `stale_source_gen_dropped` without accepting the stale slice.
    let _ = release_pre_mut.send(());
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |_| delivered_pre_mut.try_recv().is_ok(),
        "delivery of held pre-mutation async completion across live generation fence",
    )
    .await?;

    // Now release the `Err(ErrorKind::SourceChanged)` continuation and confirm idempotent count == 1.
    let _ = release_inv_read.send(());
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |_| delivered_inv_read.try_recv().is_ok(),
        "delivery of post-truncation SourceChanged completion",
    )
    .await?;

    wait_background_ops_drained(harness, true, cx, "post-SourceChanged worker drain").await?;

    // Draw real native frame and verify EVERY open view prepainted the shared invalidation banner.
    draw_frame(window, cx)?;
    cx.update(|cx| -> Result<(), String> {
        let h = harness.read(cx);
        let src = h
            .source
            .as_ref()
            .ok_or("missing source after SourceChanged")?;
        let stats = h.paging.stats();
        let err_views = h.prepainted_error_views.borrow();

        if src.source_error.is_none()
            || src.source_change_count != 1
            || !src.index_cancel.is_cancelled()
            || stats.pages != 0
            || stats.cached_bytes != 0
            || stats.sources != 0
            || err_views.len() != n
            || h.stale_source_gen_dropped == 0
            || h.views.iter().any(|v| v.current_slice.is_some() || v.is_loading)
        {
            return Err(format!(
                "all-view SourceChanged verification failed: err={:?} count={} idx_cancelled={} stats={stats:?} prepainted_err_views={}/{n} stale_gen_dropped={}",
                src.source_error,
                src.source_change_count,
                src.index_cancel.is_cancelled(),
                err_views.len(),
                h.stale_source_gen_dropped
            ));
        }

        report.source_changed_audit = SourceChangedAudit {
            sources_before_mutation: sources_before_mut,
            source_change_count: src.source_change_count,
            index_cancel_cancelled: src.index_cancel.is_cancelled(),
            pages_after_invalidation: stats.pages,
            cached_bytes_after_invalidation: stats.cached_bytes,
            sources_after_invalidation: stats.sources,
            expected_views_invalidated: n,
            prepainted_invalidated_views: err_views.len(),
            stale_source_gen_completions_dropped: h.stale_source_gen_dropped,
            frame_drawn: true,
        };
        report.source_changed_all_views_invalidated_verified = true;
        Ok(())
    })?;

    Ok(())
}

async fn open_paged_source_off_thread(
    paging: PagingService,
    doc_id: DocumentId,
    lifecycle_gen: LifecycleGen,
    title: String,
    path: PathBuf,
    editable_byte_limit: u64,
    cx: &mut AsyncApp,
) -> Result<PagedSourceState, String> {
    cx.background_executor()
        .spawn(async move {
            let cancel = CancelToken::new();
            let options = OpenOptions {
                editable_byte_limit,
                long_line_byte_limit: 1024 * 1024,
            };
            let outcome = loop {
                match editor_services::open_document(doc_id, &path, &paging, options, &cancel) {
                    Err(e) if e.kind == ErrorKind::Busy => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    other => {
                        break other.map_err(|e| {
                            format!(
                                "open_document('{}') failed ({:?}): {}",
                                path.display(),
                                e.kind,
                                e.message
                            )
                        })?;
                    }
                }
            };

            match outcome {
                OpenOutcome::Paged {
                    doc,
                    encoding,
                    line_ending,
                    readonly_reason,
                    fingerprint,
                    source_gen,
                } => {
                    let meta = DocMeta {
                        title,
                        path: Some(path),
                        encoding,
                        line_ending,
                        origin_fingerprint: Some(fingerprint),
                        saved_identity: None,
                        saved_encoding: Some(encoding),
                        saved_line_ending: Some(line_ending),
                        language_override: None,
                        readonly_reason: Some(readonly_reason.clone()),
                        paged: true,
                    };
                    Ok(PagedSourceState::new(
                        doc_id,
                        lifecycle_gen,
                        meta,
                        doc,
                        source_gen,
                        readonly_reason,
                    ))
                }
                OpenOutcome::Editable { .. } => Err(format!(
                    "expected OpenOutcome::Paged for '{}', got OpenOutcome::Editable",
                    path.display()
                )),
            }
        })
        .await
}

async fn drain_search_task_off_thread(
    task: editor_services::SearchTask,
    cx: &mut AsyncApp,
) -> Result<(Vec<SearchMatch>, u64), String> {
    cx.background_executor()
        .spawn(async move {
            let mut all_matches = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match task.events.try_recv() {
                    Ok(SearchEvent::Batch { matches, .. }) => {
                        all_matches.extend(matches);
                    }
                    Ok(SearchEvent::Done { bytes_scanned, .. }) => {
                        // Wait for SearchJob drop (`job.sender` disconnect) so the search worker's
                        // cloned `PagedDocHandle` and document slot are deterministically released.
                        while Instant::now() < deadline {
                            match task.events.try_recv() {
                                Err(TryRecvError::Disconnected) => {
                                    return Ok((all_matches, bytes_scanned));
                                }
                                _ => std::thread::sleep(Duration::from_millis(1)),
                            }
                        }
                        return Err(
                            "timed out waiting for search worker channel disconnect after Done"
                                .to_string(),
                        );
                    }
                    Ok(SearchEvent::Cancelled { .. }) => {
                        return Err("search unexpectedly cancelled".to_string());
                    }
                    Ok(SearchEvent::Invalidated { .. }) => {
                        return Err("search unexpectedly invalidated".to_string());
                    }
                    Ok(SearchEvent::Failed { error, .. }) => {
                        return Err(format!("search failed: {}", error.message));
                    }
                    Err(TryRecvError::Empty) => {
                        if Instant::now() >= deadline {
                            return Err("timed out waiting for SearchEvent::Done".to_string());
                        }
                        std::thread::sleep(Duration::from_millis(4));
                    }
                    Err(TryRecvError::Disconnected) => {
                        return Err("search event channel disconnected before Done".to_string());
                    }
                }
            }
        })
        .await
}

fn draw_frame(window: &AnyWindowHandle, cx: &mut AsyncApp) -> Result<(), String> {
    window
        .update(cx, |_, win, cx| {
            win.refresh();
            win.draw(cx).clear(cx);
        })
        .map_err(|e| format!("draw_frame failed: {e}"))
}

async fn pump_settle_frames(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    rounds: usize,
) -> Result<(), String> {
    for _ in 0..rounds {
        draw_frame(window, cx)?;
        cx.background_executor()
            .timer(Duration::from_millis(8))
            .await;
    }
    draw_frame(window, cx)
}

async fn wait_views_settled(
    window: &AnyWindowHandle,
    harness: &Entity<MultiviewPagedHarness>,
    cx: &mut AsyncApp,
    label: &str,
) -> Result<(), String> {
    wait_until(
        Duration::from_secs(10),
        window,
        cx,
        |cx| {
            cx.update(|cx| {
                let h = harness.read(cx);
                let src_err = h.source.as_ref().is_some_and(|s| s.source_error.is_some());
                src_err
                    || (!h.views.is_empty()
                        && h.views.iter().all(|v| {
                            !v.is_loading
                                && (v.current_slice.is_some() || v.viewport_error.is_some())
                        }))
            })
        },
        label,
    )
    .await
}

async fn wait_background_ops_drained(
    harness: &Entity<MultiviewPagedHarness>,
    require_zero_sources: bool,
    cx: &mut AsyncApp,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    let timeout = Duration::from_secs(5);
    loop {
        let drained = cx.update(|cx| {
            let h = harness.read(cx);
            h.in_flight_bg_ops() == 0
                && h.search.active_tasks() == 0
                && (!require_zero_sources || h.paging.stats().sources == 0)
        });
        if drained {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            let (ops, tasks, sources) = cx.update(|cx| {
                let h = harness.read(cx);
                (
                    h.in_flight_bg_ops(),
                    h.search.active_tasks(),
                    h.paging.stats().sources,
                )
            });
            return Err(format!(
                "timed out waiting for {label}: in_flight_bg_ops={ops} search_tasks={tasks} paging_sources={sources}"
            ));
        }
        cx.background_executor()
            .timer(Duration::from_millis(2))
            .await;
    }
}

async fn wait_until(
    timeout: Duration,
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    mut pred: impl FnMut(&mut AsyncApp) -> bool,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        draw_frame(window, cx)?;
        if pred(cx) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!("timed out waiting for {label}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(8))
            .await;
    }
}

fn click_point(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    pt: Point<Pixels>,
) -> Result<(), String> {
    for input in [
        PlatformInput::MouseMove(MouseMoveEvent {
            position: pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    ] {
        window
            .update(cx, |_, win, cx| {
                win.dispatch_event(input, cx);
                win.refresh();
                win.draw(cx).clear(cx);
            })
            .map_err(|e| format!("click_point dispatch failed: {e}"))?;
    }
    Ok(())
}

fn bounds_intersect_strict(a: gpui_kit::Bounds<Pixels>, b: gpui_kit::Bounds<Pixels>) -> bool {
    let eps = 0.5f32;
    let ax0 = a.origin.x.as_f32() + eps;
    let ay0 = a.origin.y.as_f32() + eps;
    let ax1 = a.origin.x.as_f32() + a.size.width.as_f32() - eps;
    let ay1 = a.origin.y.as_f32() + a.size.height.as_f32() - eps;

    let bx0 = b.origin.x.as_f32() + eps;
    let by0 = b.origin.y.as_f32() + eps;
    let bx1 = b.origin.x.as_f32() + b.size.width.as_f32() - eps;
    let by1 = b.origin.y.as_f32() + b.size.height.as_f32() - eps;

    ax0 < bx1 && ax1 > bx0 && ay0 < by1 && ay1 > by0
}

#[allow(dead_code)]
fn _assert_path_used(_: &Path, _: Size<Pixels>) {}
