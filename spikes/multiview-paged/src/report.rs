use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

use editor_services::CacheStats;
use serde::Serialize;

#[derive(Clone, Debug, Default, Serialize)]
pub struct DenseFixtureReport {
    pub byte_len: u64,
    pub expected_lines: u64,
    pub expected_search_matches: usize,
    pub purpose: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SparseFixtureReport {
    pub logical_byte_len: u64,
    pub min_high_offset_byte: u64,
    pub sparse_set_len_supported: bool,
    pub sparse_error: Option<String>,
    pub purpose: &'static str,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ResourceSnapshot {
    pub phase: &'static str,
    pub mounted_views: usize,
    pub total_cached_segments: usize,
    pub total_item_size_entries: usize,
    pub total_visible_rendered_rows: usize,
    pub paging_sources: usize,
    pub paging_pages: usize,
    pub paging_cached_bytes: usize,
    pub paging_cache_budget: usize,
    pub paging_index_bytes: usize,
    pub paging_index_budget: usize,
    pub active_open_jobs: usize,
    pub peak_open_jobs: usize,
    pub active_index_jobs: usize,
    pub peak_index_jobs: usize,
}

impl ResourceSnapshot {
    pub fn from_stats(
        phase: &'static str,
        mounted_views: usize,
        total_cached_segments: usize,
        total_item_size_entries: usize,
        total_visible_rendered_rows: usize,
        stats: CacheStats,
    ) -> Self {
        Self {
            phase,
            mounted_views,
            total_cached_segments,
            total_item_size_entries,
            total_visible_rendered_rows,
            paging_sources: stats.sources,
            paging_pages: stats.pages,
            paging_cached_bytes: stats.cached_bytes,
            paging_cache_budget: stats.cache_budget,
            paging_index_bytes: stats.index_bytes,
            paging_index_budget: stats.index_budget,
            active_open_jobs: stats.active_open_jobs,
            peak_open_jobs: stats.peak_open_jobs,
            active_index_jobs: stats.active_index_jobs,
            peak_index_jobs: stats.peak_index_jobs,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PerViewAudit {
    pub slot_index: usize,
    pub view_id: String,
    pub leaf_bounds_xywh_px: [f32; 4],
    pub vlist_bounds_xywh_px: [f32; 4],
    pub slice_byte_range: [u64; 2],
    pub captured_cursor_byte: u64,
    pub captured_scroll_byte: u64,
    pub cached_segment_count: usize,
    pub item_size_count: usize,
    pub continuation_segment_count: usize,
    pub visible_row_range_before_wheel: [usize; 2],
    pub culled_properly: bool,
    pub scroll_offset_y_before_wheel_px: f32,
    pub scroll_offset_y_after_wheel_px: f32,
    pub visible_row_range_after_wheel: [usize; 2],
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SearchOwnershipAudit {
    pub active_slots_sampled: usize,
    pub matches_emitted: usize,
    pub bytes_scanned: u64,
    pub initial_active_view_id: String,
    pub initial_highlighted_views: Vec<String>,
    pub switched_active_view_id: Option<String>,
    pub prior_view_highlight_cleared_on_switch: Option<bool>,
    pub switched_highlighted_views: Vec<String>,
    pub switched_anchor_byte: Option<u64>,
    pub non_active_anchors_unchanged: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CancelIsolationAudit {
    pub multi_view_applicable: bool,
    pub superseded_token_cancelled: bool,
    pub replacement_token_completed: bool,
    pub sibling_token_uncancelled: Option<bool>,
    pub sibling_request_completed: Option<bool>,
    pub stale_req_seq_completions_dropped: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CloseLifecycleAudit {
    pub initial_views: usize,
    pub close_to_one_applicable: bool,
    pub closed_views_count: usize,
    pub closed_view_tokens_cancelled: bool,
    pub sources_at_one_surviving_view: Option<usize>,
    pub index_cancel_cancelled_at_one_view: Option<bool>,
    pub index_complete_preserved_at_one_view: Option<bool>,
    pub index_checkpoints_at_one_view: Option<usize>,
    pub surviving_view_fresh_read_verified: Option<bool>,
    pub sources_after_last_view_closed: usize,
    pub pages_after_last_view_closed: usize,
    pub cached_bytes_after_last_view_closed: usize,
    pub index_bytes_after_last_view_closed: usize,
    pub index_cancel_cancelled_after_last_view_closed: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SparsePhaseAudit {
    pub sources_before_open: usize,
    pub sources_while_open: usize,
    pub mounted_views: usize,
    pub view_anchors_64bit: Vec<u64>,
    pub captured_scroll_bytes_64bit: Vec<u64>,
    pub rendered_gutter_samples: Vec<String>,
    pub continuation_segments_verified: bool,
    pub offset_gt_u32_max_verified: bool,
    pub frame_drawn: bool,
    pub sources_after_close: usize,
    pub pages_after_close: usize,
    pub cached_bytes_after_close: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SourceChangedAudit {
    pub sources_before_mutation: usize,
    pub source_change_count: u64,
    pub index_cancel_cancelled: bool,
    pub pages_after_invalidation: usize,
    pub cached_bytes_after_invalidation: usize,
    pub sources_after_invalidation: usize,
    pub expected_views_invalidated: usize,
    pub prepainted_invalidated_views: usize,
    pub stale_source_gen_completions_dropped: u64,
    pub frame_drawn: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExperimentReport {
    pub experiment: &'static str,
    pub configured_views: usize,
    pub segmenter_authority: &'static str,
    pub foreground_io_evidence: &'static str,
    pub rss_measurement_note: &'static str,

    pub passed: bool,
    pub failure_reason: Option<String>,

    // Native window & frame metrics
    pub process_to_window_ms: f64,
    pub window_viewport_width_px: f32,
    pub window_viewport_height_px: f32,
    pub window_scale_factor: f32,
    pub window_is_active: bool,
    pub native_frame_drawn: bool,
    pub frames_drawn_total: u64,
    pub all_views_min_bounds_240x160_verified: bool,
    pub min_leaf_width_px: f32,
    pub min_leaf_height_px: f32,
    pub min_vlist_width_px: f32,
    pub min_vlist_height_px: f32,

    // Timings (outside fixture generation)
    pub dense_source_open_ms: f64,
    pub attach_n_views_and_first_draw_ms: f64,
    pub same_offset_hydration_and_draw_ms: f64,
    pub disjoint_offset_hydration_and_draw_ms: f64,
    pub disjoint_steady_frame_draw_ms: f64,
    pub cooperative_full_index_ms: f64,
    pub native_wheel_scroll_and_draw_ms: f64,
    pub total_elapsed_ms: f64,

    // Datasets & resource snapshots
    pub dense_fixture: DenseFixtureReport,
    pub sparse_fixture: SparseFixtureReport,
    pub same_offset_stats: ResourceSnapshot,
    pub disjoint_offset_stats: ResourceSnapshot,
    pub post_index_stats: ResourceSnapshot,
    pub close_to_zero_dense_stats: ResourceSnapshot,
    pub per_view_disjoint_audit: Vec<PerViewAudit>,
    pub search_ownership_audit: SearchOwnershipAudit,
    pub cancel_isolation_audit: CancelIsolationAudit,
    pub close_lifecycle_audit: CloseLifecycleAudit,
    pub sparse_64bit_audit: SparsePhaseAudit,
    pub source_changed_audit: SourceChangedAudit,

    // Behavioral outcome gates
    pub single_source_authority_verified: bool,
    pub same_offset_cache_dedup_verified: bool,
    pub disjoint_offsets_within_budget_verified: bool,
    pub cooperative_index_complete_verified: bool,
    pub segment_virtualization_culling_verified: bool,
    pub native_wheel_independent_scroll_verified: bool,
    pub search_and_focus_highlight_ownership_verified: bool,
    pub request_cancel_isolation_verified: bool,
    pub close_n_to_1_preserved_verified: Option<bool>,
    pub close_1_to_0_released_verified: bool,
    pub sparse_64bit_offset_verified: bool,
    pub source_changed_all_views_invalidated_verified: bool,
    pub scratch_cleanup_verified: bool,

    pub unverified_limits: Vec<&'static str>,
}

impl ExperimentReport {
    pub fn new(configured_views: usize) -> Self {
        Self {
            experiment: "multiview-paged-stage3-feasibility",
            configured_views,
            segmenter_authority: "editor::paged_view::build_display_segments (public crate reuse, zero product edits)",
            foreground_io_evidence: "By source inspection (no runtime foreground-I/O instrumentation), all fixture creation, open, viewport read, index_step, search scan, disk mutation, scratch unlink, and report file read/write operations run off the GPUI foreground thread; dropping PagedSourceState on the foreground thread performs the final PagedDocHandle OS descriptor close after worker handle clones drop (zero foreground file reads or writes).",
            rss_measurement_note: "Measure process max RSS externally via OS `/usr/bin/time -l`; in-app report records retained segment/item-size counts and PagingService byte/page/source counters.",
            passed: false,
            failure_reason: None,
            process_to_window_ms: 0.0,
            window_viewport_width_px: 0.0,
            window_viewport_height_px: 0.0,
            window_scale_factor: 1.0,
            window_is_active: false,
            native_frame_drawn: false,
            frames_drawn_total: 0,
            all_views_min_bounds_240x160_verified: false,
            min_leaf_width_px: 0.0,
            min_leaf_height_px: 0.0,
            min_vlist_width_px: 0.0,
            min_vlist_height_px: 0.0,
            dense_source_open_ms: 0.0,
            attach_n_views_and_first_draw_ms: 0.0,
            same_offset_hydration_and_draw_ms: 0.0,
            disjoint_offset_hydration_and_draw_ms: 0.0,
            disjoint_steady_frame_draw_ms: 0.0,
            cooperative_full_index_ms: 0.0,
            native_wheel_scroll_and_draw_ms: 0.0,
            total_elapsed_ms: 0.0,
            dense_fixture: DenseFixtureReport {
                byte_len: 0,
                expected_lines: 0,
                expected_search_matches: 0,
                purpose: "Bounded 8 MiB UTF-8 fixture for full cooperative SparseIndex build, streaming literal SearchService scan, cache deduplication/budget, native wheel scroll/culling, request cancellation, close lifecycle, and SourceChanged invalidation.",
            },
            sparse_fixture: SparseFixtureReport {
                logical_byte_len: 0,
                min_high_offset_byte: 0,
                sparse_set_len_supported: false,
                sparse_error: None,
                purpose: "Separate >4 GiB sparse marker source solely for 64-bit (>u32::MAX) byte offset, ViewState, and gutter formatting correctness; never used for throughput, full indexing, or full-file search.",
            },
            same_offset_stats: ResourceSnapshot::default(),
            disjoint_offset_stats: ResourceSnapshot::default(),
            post_index_stats: ResourceSnapshot::default(),
            close_to_zero_dense_stats: ResourceSnapshot::default(),
            per_view_disjoint_audit: Vec::new(),
            search_ownership_audit: SearchOwnershipAudit::default(),
            cancel_isolation_audit: CancelIsolationAudit::default(),
            close_lifecycle_audit: CloseLifecycleAudit::default(),
            sparse_64bit_audit: SparsePhaseAudit::default(),
            source_changed_audit: SourceChangedAudit::default(),
            single_source_authority_verified: false,
            same_offset_cache_dedup_verified: false,
            disjoint_offsets_within_budget_verified: false,
            cooperative_index_complete_verified: false,
            segment_virtualization_culling_verified: false,
            native_wheel_independent_scroll_verified: false,
            search_and_focus_highlight_ownership_verified: false,
            request_cancel_isolation_verified: false,
            close_n_to_1_preserved_verified: None,
            close_1_to_0_released_verified: false,
            sparse_64bit_offset_verified: false,
            source_changed_all_views_invalidated_verified: false,
            scratch_cleanup_verified: false,
            unverified_limits: vec![
                "No physical display/Metal compositor pixel capture or Retina glyph contrast proof (`window.draw` + hitbox/bounds/virtual_list probes verify GPUI layout, culling, and paint closure execution only).",
                "No physical OS HID mouse wheel or NSScrollView trackpad momentum gesture routing through macOS WindowServer (`Window::dispatch_event(PlatformInput::Scroll)` injects synthetic window-level events).",
                "No physical CJK/system IME NSTextInputClient marked-text composition or candidate window proof.",
                "Sparse >4 GiB marker fixture verifies 64-bit (>u32::MAX) offset math, ViewState capture, and gutter formatting only; zero sparse I/O throughput claim.",
                "Ephemeral in-memory multiview harness only; production persisted multiview remains gated on SQLite Schema v3 (`recovery_layout.rs`), `SessionLayout` duplicate-DocumentId lifecycle rules, and shared Editable multiview.",
            ],
        }
    }

    pub fn all_gates_satisfied(&self) -> bool {
        let close_n_to_1_ok = if self.configured_views > 1 {
            self.close_n_to_1_preserved_verified == Some(true)
        } else {
            self.close_n_to_1_preserved_verified.is_none()
        };

        self.native_frame_drawn
            && self.all_views_min_bounds_240x160_verified
            && self.single_source_authority_verified
            && self.same_offset_cache_dedup_verified
            && self.disjoint_offsets_within_budget_verified
            && self.cooperative_index_complete_verified
            && self.segment_virtualization_culling_verified
            && self.native_wheel_independent_scroll_verified
            && self.search_and_focus_highlight_ownership_verified
            && self.request_cancel_isolation_verified
            && close_n_to_1_ok
            && self.close_1_to_0_released_verified
            && self.sparse_64bit_offset_verified
            && self.source_changed_all_views_invalidated_verified
            && self.scratch_cleanup_verified
            && self.failure_reason.is_none()
    }
}

/// Worker-only (or pre-GPUI CLI error): writes pretty JSON to `path` (if provided) and emits a
/// single-line summary record to stdout/stderr, flushing both streams before process termination.
pub fn write_and_flush_report(
    report: &ExperimentReport,
    path: Option<&Path>,
) -> Result<(), String> {
    let pretty = serde_json::to_string_pretty(report)
        .map_err(|e| format!("failed to serialize ExperimentReport: {e}"))?;
    let compact = serde_json::to_string(report)
        .map_err(|e| format!("failed to serialize compact ExperimentReport: {e}"))?;

    println!("MULTIVIEW_PAGED_REPORT_JSON:{compact}");
    if !report.passed {
        eprintln!(
            "MULTIVIEW_PAGED_FAILURE: {}",
            report
                .failure_reason
                .as_deref()
                .unwrap_or("experiment failed")
        );
    }
    let stdout_flush = std::io::stdout()
        .flush()
        .map_err(|e| format!("stdout flush failed: {e}"));
    let stderr_flush = std::io::stderr()
        .flush()
        .map_err(|e| format!("stderr flush failed: {e}"));

    if let Some(target) = path {
        if let Some(parent) = target.parent()
            && !parent.as_os_str().is_empty()
        {
            if let Err(e) = std::fs::create_dir_all(parent) {
                let msg = format!(
                    "failed to create report parent directory '{}': {e}",
                    parent.display()
                );
                eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {msg}");
                let _ = std::io::stderr().flush();
                return Err(msg);
            }
        }
        if let Err(e) = std::fs::write(target, &pretty) {
            let msg = format!("failed to write report '{}': {e}", target.display());
            eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {msg}");
            let _ = std::io::stderr().flush();
            return Err(msg);
        }
    }

    stdout_flush?;
    stderr_flush?;
    Ok(())
}

/// Worker-only: resolves a `--report` target path by canonicalizing its deepest existing ancestor
/// and appending any non-existent trailing path segments, rejecting:
/// - empty / directory-only targets,
/// - `.` or `..` components appearing after a non-existent path segment (ambiguous traversal),
/// - dangling symlinks anywhere along the path,
/// - non-directory ancestors (e.g. `/dev/null/fail.json`).
pub fn resolve_report_target_canonical(raw_report: &Path) -> Result<PathBuf, String> {
    let raw_lossy = raw_report.as_os_str().to_string_lossy();
    if raw_lossy.is_empty() {
        return Err("--report path must not be empty".to_string());
    }
    if raw_lossy.ends_with('/') || raw_lossy.ends_with(std::path::MAIN_SEPARATOR) {
        return Err(format!(
            "invalid --report path '{}': trailing directory separator requires a file target",
            raw_report.display()
        ));
    }
    let anchored = if raw_report.is_absolute() {
        raw_report.to_path_buf()
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("failed to resolve current_dir for --report: {e}"))?;
        cwd.join(raw_report)
    };

    let mut cur = PathBuf::new();
    let mut missing_tail: Vec<OsString> = Vec::new();
    let mut entered_missing = false;

    for comp in anchored.components() {
        match comp {
            Component::Prefix(p) => {
                cur.push(p.as_os_str());
            }
            Component::RootDir => {
                cur.push(comp.as_os_str());
            }
            Component::CurDir => {
                if entered_missing {
                    return Err(format!(
                        "ambiguous --report path '{}': '.' component appears after non-existent segment",
                        raw_report.display()
                    ));
                }
            }
            Component::ParentDir => {
                if entered_missing {
                    return Err(format!(
                        "ambiguous --report path '{}': '..' component appears after non-existent segment",
                        raw_report.display()
                    ));
                }
                cur.push("..");
                cur = std::fs::canonicalize(&cur).map_err(|e| {
                    format!(
                        "cannot canonicalize '--report' ancestor '{}': {e}",
                        cur.display()
                    )
                })?;
            }
            Component::Normal(seg) => {
                if entered_missing {
                    missing_tail.push(seg.to_os_string());
                } else {
                    let candidate = cur.join(seg);
                    match std::fs::symlink_metadata(&candidate) {
                        Ok(sym_meta) => {
                            if sym_meta.file_type().is_symlink()
                                && std::fs::metadata(&candidate).is_err()
                            {
                                return Err(format!(
                                    "invalid --report path '{}': encounters dangling symlink '{}'",
                                    raw_report.display(),
                                    candidate.display()
                                ));
                            }
                            cur = std::fs::canonicalize(&candidate).map_err(|e| {
                                format!(
                                    "failed to canonicalize '--report' segment '{}': {e}",
                                    candidate.display()
                                )
                            })?;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            let cur_meta = std::fs::metadata(&cur).map_err(|me| {
                                format!("cannot stat '--report' ancestor '{}': {me}", cur.display())
                            })?;
                            if !cur_meta.is_dir() {
                                return Err(format!(
                                    "invalid --report path '{}': ancestor '{}' is not a directory",
                                    raw_report.display(),
                                    cur.display()
                                ));
                            }
                            entered_missing = true;
                            missing_tail.push(seg.to_os_string());
                        }
                        Err(e) => {
                            return Err(format!(
                                "invalid --report path '{}' at '{}': {e}",
                                raw_report.display(),
                                candidate.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    let mut resolved = std::fs::canonicalize(&cur).map_err(|e| {
        format!(
            "failed to canonicalize '--report' base '{}': {e}",
            cur.display()
        )
    })?;

    if missing_tail.is_empty() {
        let meta = std::fs::metadata(&resolved).map_err(|e| {
            format!(
                "failed to stat existing --report target '{}': {e}",
                resolved.display()
            )
        })?;
        if meta.is_dir() {
            return Err(format!(
                "invalid --report path '{}': target is an existing directory",
                raw_report.display()
            ));
        }
    } else {
        for seg in missing_tail {
            resolved.push(seg);
        }
    }

    Ok(resolved)
}
