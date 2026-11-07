use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use editor_services::{
    DocMeta, DocumentId, EncodingSpec, FaultInjector, LifecycleGen, LineEnding, RecoveryHandle,
    Revision, SearchAlgorithm, SessionTabStub, UiShellStore, ViewState,
};
use gpui_kit::component::input::InputState;
use gpui_kit::{AnyWindowHandle, AsyncApp, Entity, EntityInputHandler as _, Focusable as _};
use serde::Serialize;

use crate::actions::{
    DuplicateLine, MoveLineDown, MoveLineUp, SaveFile, ToggleComment, ToggleLineEnding,
    ToggleThemeMode, ToggleUtf8Bom,
};
use crate::app::{EditorWorkspace, StubHydrationSource, TabSlot, classify_stub_hydration_source};
use crate::cli::SmokeScenario;
use crate::editor_adapter::{ApplyEditsError, compute_metadata_dirty};
use crate::search_bar::{SearchQueryKind, SearchStatus};
use crate::shell::EditorViewPrefs;
pub const TYPING_LATENCY_LABEL: &str = "submission-only, not screen latency";
pub const TYPING_BENCH_OPS: usize = 128;

/// Structured JSON report emitted by `--smoke` AFTER application shutdown completes
/// (never logs full document contents and never marks `passed: true` before durable shutdown ACK).
#[derive(Clone, Debug, Default, Serialize)]
pub struct SmokeReport {
    pub scenario: String,
    pub in_app_checks_passed: bool,
    pub clean_shutdown_requested: bool,
    pub shutdown_durable_ack_observed: bool,
    pub passed: bool,
    pub failure_reason: Option<String>,
    pub process_to_window_ms: f64,
    pub first_interactive_ms: Option<f64>,
    pub recovery_manifest_ms: Option<f64>,
    pub total_elapsed_ms: f64,
    pub final_tab_count: usize,
    pub hydrated_editor_count: usize,
    pub hydrated_content_bytes: u64,
    pub paged_tab_count: usize,
    pub committed_revisions_observed: Vec<u64>,
    pub typing_latency_label: String,
    pub typing_doc_bytes: u64,
    pub typing_sample_count: usize,
    pub typing_submit_p50_ms: f64,
    pub typing_submit_p95_ms: f64,
    pub typing_submit_max_ms: f64,
    pub large_paste_64k_submit_ms: Option<f64>,
    pub atomic_batch_single_undo_verified: bool,
    pub stale_revision_rejected: bool,
    pub chained_programmatic_edits_verified: bool,
    pub pending_native_edit_guard_verified: bool,
    pub replace_preserves_done_search_verified: bool,
    pub unicode_search_verified: bool,
    pub crlf_bom_roundtrip_verified: bool,
    pub reopen_closed_tab_verified: bool,
    pub discard_reload_durable_verified: bool,
    pub post_shutdown_manifest_verified: bool,
    pub clean_untitled_restart_verified: bool,
    pub paged_streaming_search_matches: usize,
    pub kernel_parity_verified: bool,
    pub palette_undo_select_all_focus_verified: bool,
    pub settings_nondirty_and_lazy_apply_verified: bool,
    pub switcher_metadata_only_verified: bool,
    pub crlf_line_ops_single_undo_verified: bool,
    pub empty_untitled_prefs_layout_verified: bool,
    pub format_flip_back_and_inflight_save_verified: bool,
    pub format_only_bom_restart_and_save_verified: bool,
    pub mru_persistence_verified: bool,
    pub shell_persistence_warning: Option<String>,
    #[serde(skip)]
    pub expected_post_shutdown_fresh_doc: Option<DocumentId>,
    #[serde(skip)]
    pub tombstoned_old_doc: Option<DocumentId>,
    #[serde(skip)]
    pub expected_clean_untitled_doc: Option<DocumentId>,
    #[serde(skip)]
    pub expected_usability_empty_untitled_doc: Option<DocumentId>,
    #[serde(skip)]
    pub expected_usability_bom_dirty_doc: Option<DocumentId>,
    #[serde(skip)]
    pub expected_usability_mru_paths: Vec<PathBuf>,
}

#[derive(Clone, Default)]
pub struct SharedSmokeOutcome(Arc<Mutex<Option<SmokeReport>>>);

impl SharedSmokeOutcome {
    pub fn store(&self, report: SmokeReport) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    }

    pub fn take(&self) -> Option<SmokeReport> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

pub fn spawn_smoke_runner(
    scenario: SmokeScenario,
    window_handle: AnyWindowHandle,
    workspace: Entity<EditorWorkspace>,
    outcome_slot: SharedSmokeOutcome,
    cx: &mut gpui_kit::App,
) {
    cx.spawn(async move |cx: &mut AsyncApp| {
        let started = Instant::now();
        let smoke_report_path = cx.update(|cx| workspace.read(cx).cli.smoke_report.clone());
        let mut report = SmokeReport {
            scenario: match scenario {
                SmokeScenario::StartupCheck => "startup".to_string(),
                SmokeScenario::Full => "full".to_string(),
                SmokeScenario::Usability => "usability".to_string(),
            },
            typing_latency_label: TYPING_LATENCY_LABEL.to_string(),
            ..Default::default()
        };

        let run_result = match scenario {
            SmokeScenario::StartupCheck => {
                run_startup_scenario(&window_handle, &workspace, &mut report, cx).await
            }
            SmokeScenario::Full => {
                run_full_scenario(&window_handle, &workspace, &mut report, cx).await
            }
            SmokeScenario::Usability => {
                run_usability_scenario(&window_handle, &workspace, &mut report, cx).await
            }
        };
        cx.update(|cx| {
            capture_workspace_metrics(workspace.read(cx), &mut report);
        });
        report.total_elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        match run_result {
            Ok(()) => {
                report.in_app_checks_passed = true;
            }
            Err(err) => {
                report.in_app_checks_passed = false;
                report.failure_reason = Some(err);
            }
        }

        // Guard 1: Store report into `outcome_slot` BEFORE calling `initiate_clean_quit`.
        // On macOS AppKit, `[NSApp terminate:nil]` inside `cx.quit()` terminates the process
        // directly without unwinding `application().run(...)`, so `initiate_clean_quit` finalizes,
        // emits, and flushes the report ONLY AFTER primary `RecoveryHandle::shutdown()` ACKs
        // (and sequential post-shutdown SQLite verification finishes) and BEFORE `cx.quit()`.
        report.clean_shutdown_requested = true;
        outcome_slot.store(report.clone());

        let quit_started = window_handle.update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.initiate_clean_quit(!report.in_app_checks_passed, window, cx);
                ws.shutting_down
            })
        });

        let quit_failure = match quit_started {
            Ok(true) => None,
            Ok(false) => {
                Some("smoke quit was blocked before entering clean shutdown barrier".to_string())
            }
            Err(e) => Some(format!("window update failed during smoke quit: {e}")),
        };
        if let Some(reason) = quit_failure {
            let mut failed = outcome_slot.take().unwrap_or(report);
            failed.passed = false;
            failed.shutdown_durable_ack_observed = false;
            if failed.failure_reason.is_none() {
                failed.failure_reason = Some(reason);
            }
            emit_smoke_report(&failed, smoke_report_path.as_deref());
            std::process::exit(1);
        }
    })
    .detach();
}

pub fn emit_smoke_report(report: &SmokeReport, path: Option<&Path>) {
    if let Ok(json) = serde_json::to_string_pretty(report) {
        if let Some(p) = path {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(p, &json);
        }
        println!(
            "SMOKE_REPORT_JSON:{}",
            serde_json::to_string(report).unwrap_or_default()
        );
    }
    if !report.passed {
        eprintln!(
            "SMOKE_FAILURE: {}",
            report
                .failure_reason
                .as_deref()
                .unwrap_or("smoke scenario failed")
        );
    }
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let _ = std::io::Write::flush(&mut std::io::stderr());
}

async fn run_startup_scenario(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                let active_hydrated = ws
                    .tabs
                    .get(ws.active_tab)
                    .is_some_and(|s| !matches!(s, TabSlot::Hydrating { .. }));
                ws.recovery_ready && active_hydrated
            })
        },
        "recovery bootstrap and active tab hydration",
    )
    .await?;

    // Pump real window draw frames so Metal layout/text shaping executes.
    let _ = window_handle.update(cx, |_, window, cx| {
        window.refresh();
        let _ = window.draw(cx);
    });
    sleep_ms(40, cx).await;

    cx.update(|cx| {
        let ws = workspace.read(cx);
        capture_workspace_metrics(ws, report);
    });

    Ok(())
}

async fn run_full_scenario(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    // 1. Wait for bootstrap & create a fresh clean untitled editable tab for deterministic testing.
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                let active_ready = ws
                    .tabs
                    .get(ws.active_tab)
                    .is_some_and(|s| !matches!(s, TabSlot::Hydrating { .. }));
                ws.recovery_ready && active_ready
            })
        },
        "recovery bootstrap",
    )
    .await?;

    let raw_work_dir = cx.update(|cx| workspace.read(cx).cli.data_dir.join("smoke_artifacts"));
    std::fs::create_dir_all(&raw_work_dir).map_err(|e| format!("create smoke_artifacts: {e}"))?;
    let work_dir = std::fs::canonicalize(&raw_work_dir).unwrap_or(raw_work_dir);

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.create_untitled_tab(true, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    // Draw frame so `window.rendered_frame.dispatch_tree` registers the focused `Editor`'s FocusId
    // before `window.dispatch_action(Undo/Redo)` runs.
    pump_frame(window_handle, cx)?;

    // 2. Real Kit `EntityInputHandler::replace_text_in_range` + Undo + Redo.
    // Note: GPUI `App::finish_update` flushes `Effect::Emit` and `Effect::Defer` (`dispatch_action`)
    // synchronously when the outermost `window_handle.update(...)` exits (`pending_updates: 1 -> 0`).
    let initial_sample = "fn greet() {\n    let label = \"你好 GPUI 🚀\";\n}\n";
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "active tab is not Editable".to_string())?;
                let editor_entity = ed.editor().clone();
                editor_entity.update(cx, |state, cx| {
                    state.replace_text_in_range(None, initial_sample, window, cx);
                });
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing active editable tab after replace_text_in_range".to_string());
        };
        let rev_after_type = ed.visible_revision;
        report.committed_revisions_observed.push(rev_after_type.0);
        if rev_after_type.0 < 1 || !ed.is_dirty() {
            return Err(format!(
                "expected dirty rev >= 1 after replace_text_in_range, got rev={} dirty={}",
                rev_after_type.0,
                ed.is_dirty()
            ));
        }
        if ed.committed_byte_len() != initial_sample.len() as u64 {
            return Err(format!(
                "expected {} committed bytes, got {}",
                initial_sample.len(),
                ed.committed_byte_len()
            ));
        }
        Ok(())
    })?;

    // Undo -> empty untitled buffer must clear dirty immediately!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing active editable tab for undo".to_string())?;
                ed.undo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing active editable tab after undo".to_string());
        };
        report
            .committed_revisions_observed
            .push(ed.visible_revision.0);
        if ed.committed_byte_len() != 0 || ed.is_dirty() {
            return Err(format!(
                "undo failed: len={} dirty={}",
                ed.committed_byte_len(),
                ed.is_dirty()
            ));
        }
        Ok(())
    })?;

    // Redo -> restores committed bytes and dirty flag.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing active editable tab for redo".to_string())?;
                ed.redo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing active editable tab after redo".to_string());
        };
        report
            .committed_revisions_observed
            .push(ed.visible_revision.0);
        if ed.committed_byte_len() != initial_sample.len() as u64 || !ed.is_dirty() {
            return Err("redo failed to restore committed bytes and dirty state".to_string());
        }
        Ok(())
    })?;

    // 3. Atomic Multi-Edit Batch + Stale Revision Rejection + Single-Step Undo (`F09`, `F10`).
    let expected_after_batch = "pub fn welcome() {\n    let label = \"你好 Rust GPUI 🚀\";\n}\n";
    let new_rev = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<Revision, String> {
                let base_rev = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab".to_string())?
                    .visible_revision;

                // Stale revision must be rejected atomically without mutating buffer.
                let stale_err = ws.apply_active_atomic_edits(
                    Revision(base_rev.0 + 99),
                    &[(0..2, "pub fn".to_string())],
                    window,
                    cx,
                );
                if !matches!(stale_err, Err(ApplyEditsError::StaleRevision { .. })) {
                    return Err(format!("expected StaleRevision error, got {stale_err:?}"));
                }
                report.stale_revision_rejected = true;

                // Apply two non-overlapping edits across UTF-8 multibyte text in one atomic transaction:
                let needle_lit = "\"你好 GPUI 🚀\"";
                let label_pos = initial_sample
                    .find(needle_lit)
                    .ok_or_else(|| "missing literal in sample".to_string())?;
                let label_end = label_pos + needle_lit.len();
                let rev = ws
                    .apply_active_atomic_edits(
                        base_rev,
                        &[
                            (0..8, "pub fn welcome".to_string()),
                            (label_pos..label_end, "\"你好 Rust GPUI 🚀\"".to_string()),
                        ],
                        window,
                        cx,
                    )
                    .map_err(|e| format!("apply_active_atomic_edits failed: {e}"))?;

                if rev != Revision(base_rev.0 + 1) {
                    return Err(format!(
                        "atomic batch must advance revision by exactly 1: base={} new={}",
                        base_rev.0, rev.0
                    ));
                }
                Ok(rev)
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after atomic batch".to_string());
        };
        if ed.visible_revision != new_rev {
            return Err(format!(
                "expected visible_revision={} after atomic batch flush, got {}",
                new_rev.0, ed.visible_revision.0
            ));
        }
        report.committed_revisions_observed.push(new_rev.0);
        if ed.committed_byte_len() != expected_after_batch.len() as u64 {
            return Err(format!(
                "committed length mismatch after atomic batch: got {} expected {}",
                ed.committed_byte_len(),
                expected_after_batch.len()
            ));
        }
        Ok(())
    })?;

    // Single Undo must revert BOTH edits together back to `initial_sample`.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab for batch undo".to_string())?;
                ed.undo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after batch undo".to_string());
        };
        if ed.committed_byte_len() != initial_sample.len() as u64 {
            return Err(format!(
                "single undo after atomic batch did not revert full batch: len={} expected={}",
                ed.committed_byte_len(),
                initial_sample.len()
            ));
        }
        Ok(())
    })?;

    // Redo back to atomic batch result before Unicode search & saving.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab for batch redo".to_string())?;
                ed.redo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after batch redo".to_string());
        };
        if ed.committed_byte_len() != expected_after_batch.len() as u64 {
            return Err("redo after atomic batch did not restore full batch".to_string());
        }
        report
            .committed_revisions_observed
            .push(ed.visible_revision.0);
        report.atomic_batch_single_undo_verified = true;
        Ok(())
    })?;
    // 3b. Editable Unicode Streaming Search verification (`"你好 Rust GPUI 🚀"`).
    let unicode_hits = run_search_and_wait(
        window_handle,
        workspace,
        "你好 Rust GPUI 🚀",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;
    if unicode_hits != 1 {
        return Err(format!(
            "expected 1 Unicode search match in editable buffer, got {unicode_hits}"
        ));
    }

    // 3c. A2 Check 1: Two chained length-changing `apply_active_atomic_edits` calls inside the
    // SAME outer `window_handle.update` (immediate in-update revision/rope + post-flush revision)
    // plus identity `NoOp` invariance!
    let (chained_r0, chained_r2) = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(Revision, Revision), String> {
                let r0 = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab for chained edits".to_string())?
                    .visible_revision;

                // Call #1: "pub fn" (6 bytes) -> "pub(crate) fn" (13 bytes)
                let r1 = ws
                    .apply_active_atomic_edits(
                        r0,
                        &[(0..6, "pub(crate) fn".to_string())],
                        window,
                        cx,
                    )
                    .map_err(|e| format!("chained call #1 failed: {e}"))?;
                let ed1 = ws.active_editable_mut().unwrap();
                if r1 != Revision(r0.0 + 1)
                    || ed1.visible_revision != r1
                    || ed1.committed_byte_len() != (expected_after_batch.len() + 7) as u64
                {
                    return Err(format!(
                        "chained call #1 immediate state mismatch: r1={} vis={} len={}",
                        r1.0,
                        ed1.visible_revision.0,
                        ed1.committed_byte_len()
                    ));
                }

                // Call #2 in SAME outer update: "pub(crate) fn" (13 bytes) -> "pub fn" (6 bytes)
                let r2 = ws
                    .apply_active_atomic_edits(r1, &[(0..13, "pub fn".to_string())], window, cx)
                    .map_err(|e| format!("chained call #2 failed: {e}"))?;
                let ed2 = ws.active_editable_mut().unwrap();
                if r2 != Revision(r0.0 + 2)
                    || ed2.visible_revision != r2
                    || ed2.committed_byte_len() != expected_after_batch.len() as u64
                {
                    return Err(format!(
                        "chained call #2 immediate state mismatch: r2={} vis={} len={}",
                        r2.0,
                        ed2.visible_revision.0,
                        ed2.committed_byte_len()
                    ));
                }

                // Identity NoOp in SAME outer update must leave revision unchanged at `r2`.
                let r_noop = ws
                    .apply_active_atomic_edits(r2, &[(0..6, "pub fn".to_string())], window, cx)
                    .map_err(|e| format!("identity noop failed: {e}"))?;
                if r_noop != r2 || ws.active_editable_mut().unwrap().visible_revision != r2 {
                    return Err("identity NoOp unexpectedly advanced visible_revision".to_string());
                }

                Ok((r0, r2))
            })
        })
        .map_err(|e| e.to_string())??;

    // Verify after outer `window_handle.update` flushed both queued `InputEditsEvent`s:
    // `visible_revision` must remain `chained_r2 == chained_r0 + 2` (never double-incremented!).
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after chained flush".to_string());
        };
        if ed.visible_revision != chained_r2 || chained_r2 != Revision(chained_r0.0 + 2) {
            return Err(format!(
                "post-flush chained revision mismatch: got {} expected {}",
                ed.visible_revision.0, chained_r2.0
            ));
        }
        Ok(())
    })?;
    report.chained_programmatic_edits_verified = true;

    // 3d. A2 Check 2: Equal-byte-length unflushed native edit (`"pub"` -> `"PUB"`) in same outer
    // update MUST reject programmatic `apply_active_atomic_edits` with `PendingNativeEdit`, then
    // succeed on retry once outer update flushes the native edit!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let editor_entity = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab".to_string())?
                    .editor()
                    .clone();
                editor_entity.update(cx, |state, cx| {
                    state.replace_text_in_range(Some(0..3), "PUB", window, cx);
                });
                let res = ws.apply_active_atomic_edits(
                    chained_r2,
                    &[(0..3, "pub".to_string())],
                    window,
                    cx,
                );
                if res != Err(ApplyEditsError::PendingNativeEdit) {
                    return Err(format!(
                        "expected PendingNativeEdit after equal-length unflushed native edit, got {res:?}"
                    ));
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    // Outer update exited -> native `"PUB"` edit flushed (`visible_revision == chained_r2 + 1`); retry now succeeds!
    let retry_rev = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<Revision, String> {
                let after_native_rev = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab after native flush".to_string())?
                    .visible_revision;
                if after_native_rev != Revision(chained_r2.0 + 1) {
                    return Err(format!(
                        "unmatched native edit was not ingested cleanly: got rev {} expected {}",
                        after_native_rev.0,
                        chained_r2.0 + 1
                    ));
                }
                ws.apply_active_atomic_edits(
                    after_native_rev,
                    &[(0..3, "pub".to_string())],
                    window,
                    cx,
                )
                .map_err(|e| format!("post-flush retry failed: {e}"))
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after retry flush".to_string());
        };
        if ed.visible_revision != retry_rev || retry_rev != Revision(chained_r2.0 + 2) {
            return Err(format!(
                "retry post-flush revision mismatch: got {} expected {}",
                ed.visible_revision.0, retry_rev.0
            ));
        }
        Ok(())
    })?;
    report.pending_native_edit_guard_verified = true;

    // 3e. A2 Check 3: Real `replace_all_matches` -> post-replace `run_search_from_inputs` searches
    // the NEW replaced buffer (`0` remaining matches for `"你好 Rust GPUI 🚀"`) and ends in
    // `SearchStatus::Done` (NEVER `SearchStatus::Invalidated` when `InputEditsEvent` flushes!).
    let _ = run_search_and_wait(
        window_handle,
        workspace,
        "你好 Rust GPUI 🚀",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let replace_input = ws.search_replace_input.clone();
                replace_input.update(cx, |s, cx| {
                    s.set_value("你好 GPUI Atomic 🚀", window, cx);
                });
                ws.replace_all_matches(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).search.is_running()),
        "post-replace streaming search completion",
    )
    .await?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        match &ws.search.status {
            SearchStatus::Done {
                total_emitted: 0, ..
            } => Ok(()),
            other => Err(format!(
                "expected post-replace SearchStatus::Done(total_emitted=0), got {other:?}"
            )),
        }
    })?;
    report.replace_preserves_done_search_verified = true;

    // 4. Toggle UTF-8 BOM + CRLF and Save to disk (`F02`, `F03`).
    let saved_file = work_dir.join("saved_roundtrip.rs");
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.on_toggle_bom(&ToggleUtf8Bom, window, cx);
                ws.on_toggle_line_ending(&ToggleLineEnding, window, cx);
                ws.save_tab(
                    ws.active_tab,
                    Some(saved_file.clone()),
                    true,
                    false,
                    window,
                    cx,
                );
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if !ed.is_dirty() && ed.meta.path.as_deref() == Some(saved_file.as_path())
                )
            })
        },
        "atomic save completion and savepoint clear",
    )
    .await?;

    let raw_saved = std::fs::read(&saved_file).map_err(|e| format!("read saved_file: {e}"))?;
    if !raw_saved.starts_with(&[0xEF, 0xBB, 0xBF]) || !raw_saved.windows(2).any(|w| w == b"\r\n") {
        return Err("saved file missing UTF-8 BOM or CRLF line endings".to_string());
    }
    report.crlf_bom_roundtrip_verified = true;

    // 5. Close saved tab (durable close_document ACK barrier) and reopen from Recently Closed.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let active = ws.active_tab;
                ws.request_close_tab(active, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).recently_closed.is_empty()),
        "durable tab close acknowledgement and recently_closed insertion",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.reopen_recently_closed(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.meta.path.as_deref() == Some(saved_file.as_path())
                            && ed.meta.encoding == EncodingSpec::Utf8Bom
                            && ed.meta.line_ending == LineEnding::Crlf
                )
            })
        },
        "reopen recently closed tab with preserved BOM and CRLF",
    )
    .await?;
    report.reopen_closed_tab_verified = true;

    // 5b. U-B6b: Persist local draft on `old_doc_id`, discard & reload from disk (tombstoning
    // `old_doc_id` as `state = 2` and migrating slot to a fresh `DocumentId` at `LifecycleGen(1)`),
    // then edit `fresh_doc_id` and verify durable ACK + post-shutdown manifest persistence!
    let old_doc_id = cx.update(|cx| {
        let ws = workspace.read(cx);
        ws.tabs[ws.active_tab].doc_id()
    });
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) {
                    let editor = ed.editor().clone();
                    editor.update(cx, |state, cx| {
                        state.replace_text_in_range(
                            Some(0..0),
                            "// transient draft to discard\r\n",
                            window,
                            cx,
                        );
                    });
                }
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.doc_id == old_doc_id
                            && ed.is_dirty()
                            && ed.durable_revision >= ed.visible_revision
                            && ed.durable_revision.0 > 0
                )
            })
        },
        "durable checkpoint of old_doc_id before discard-reload",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.request_reload_active(true, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.doc_id != old_doc_id
                            && ed.lifecycle_gen == LifecycleGen(1)
                            && !ed.is_dirty()
                )
            })
        },
        "discard-reload completion onto fresh DocumentId(LifecycleGen(1))",
    )
    .await?;

    let fresh_doc_id = cx.update(|cx| {
        let ws = workspace.read(cx);
        ws.tabs[ws.active_tab].doc_id()
    });

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) {
                    let editor = ed.editor().clone();
                    editor.update(cx, |state, cx| {
                        state.replace_text_in_range(
                            Some(0..0),
                            "// post-reload edit on fresh doc_id\r\n",
                            window,
                            cx,
                        );
                    });
                }
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.doc_id == fresh_doc_id
                            && ed.is_dirty()
                            && ed.durable_revision >= ed.visible_revision
                            && ed.durable_revision.0 > 0
                )
            })
        },
        "durable commit ACK on reloaded fresh_doc_id",
    )
    .await?;

    report.discard_reload_durable_verified = true;
    report.tombstoned_old_doc = Some(old_doc_id);
    report.expected_post_shutdown_fresh_doc = Some(fresh_doc_id);

    // 6. Representative 1 MiB Ordinary Snapshot Typing Submission Benchmark
    // (128 synchronous insert/delete/undo operations with RecoveryHandle active; ZERO sleeps inside timed samples).
    run_1mib_ordinary_typing_benchmark(window_handle, workspace, &work_dir, report, cx).await?;

    // 7. Paged Large-File Open + Unicode Streaming Search in both Kernels + Match Jump (`L01..L05`, `S01..S05`).
    let paged_path = work_dir.join("paged_stream.log");
    let expected_needles = write_deterministic_paged_fixture(&paged_path, 36_000)
        .map_err(|e| format!("write paged fixture: {e}"))?;

    let editors_before_paged = cx.update(|cx| workspace.read(cx).hydrated_editor_count());

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                // Route files > 512 KiB into Paged mode for deterministic fast smoke verification.
                ws.cli.editable_byte_limit = Some(512 * 1024);
                ws.open_path(paged_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Paged(pv)) if pv.current_slice.is_some()
                )
            })
        },
        "paged file first 64 KiB viewport load",
    )
    .await?;

    let editors_after_paged = cx.update(|cx| workspace.read(cx).hydrated_editor_count());
    if editors_after_paged != editors_before_paged {
        return Err("opening Paged tab unexpectedly allocated an EditorState".to_string());
    }

    // Search Kernel 1: MemchrBaseline (with multibyte UTF-8 Unicode needle)
    let memchr_count = run_search_and_wait(
        window_handle,
        workspace,
        "SMOKE_你好_NEEDLE",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;

    // Search Kernel 2: ByteScanExperiment (same multibyte UTF-8 Unicode needle)
    let alt_count = run_search_and_wait(
        window_handle,
        workspace,
        "SMOKE_你好_NEEDLE",
        SearchAlgorithm::ByteScanExperiment,
        cx,
    )
    .await?;

    if memchr_count != expected_needles || alt_count != expected_needles {
        return Err(format!(
            "paged streaming search mismatch: memchr={memchr_count}, alt={alt_count}, expected={expected_needles}"
        ));
    }
    report.unicode_search_verified = true;
    report.paged_streaming_search_matches = memchr_count;
    report.kernel_parity_verified = true;

    // Jump to last match near EOF (> 1 MiB) and verify viewport moves there.
    let last_ix = memchr_count.saturating_sub(1);
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.jump_to_search_match(last_ix, false, window, cx);
                ws.on_toggle_theme(&ToggleThemeMode, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Paged(pv)) if pv.visible_byte_range().start > 1_000_000
                )
            })
        },
        "paged viewport jump beyond 1 MiB",
    )
    .await?;

    // 8. Clean Empty Untitled Reconstitution + Session Layout Persistence (`!has_checkpoint && path.is_none()`):
    // Create an untouched empty untitled tab, exercise `start_hydrating_stub` on its layout stub
    // to verify it reconstitutes directly as `TabSlot::Editable` without querying `recovery_heads`,
    // and leave it open so `verify_post_shutdown_manifest` verifies its persisted layout stub.
    let clean_untitled_id = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<DocumentId, String> {
                ws.cli.editable_byte_limit = None;
                let id = ws.create_untitled_tab(true, window, cx);
                let idx = ws.active_tab;
                let meta = ws.tabs[idx].meta().clone();
                let stub = SessionTabStub {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(1),
                    durable_rev: Revision(0),
                    meta,
                    view: ViewState {
                        cursor_byte: 0,
                        scroll_byte: 0,
                    },
                    has_checkpoint: false,
                };
                ws.start_hydrating_stub(idx, stub, window, cx);
                match &ws.tabs[idx] {
                    TabSlot::Editable(ed)
                        if ed.doc_id == id
                            && ed.lifecycle_gen == LifecycleGen(1)
                            && !ed.is_dirty()
                            && ed.committed_byte_len() == 0 =>
                    {
                        Ok(id)
                    }
                    other => Err(format!(
                        "clean untitled stub failed direct reconstitution: got {:?}",
                        other.title()
                    )),
                }
            })
        })
        .map_err(|e| e.to_string())??;
    report.expected_clean_untitled_doc = Some(clean_untitled_id);

    pump_frame(window_handle, cx)?;

    cx.update(|cx| {
        let ws = workspace.read(cx);
        capture_workspace_metrics(ws, report);
    });

    Ok(())
}

async fn run_1mib_ordinary_typing_benchmark(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    work_dir: &Path,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let raw_bench_path = work_dir.join("ordinary_1mib_bench.rs");
    let written_bytes = write_deterministic_1mib_rust_fixture(&raw_bench_path)
        .map_err(|e| format!("write 1MiB fixture: {e}"))?;
    let bench_path = std::fs::canonicalize(&raw_bench_path).unwrap_or(raw_bench_path);

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.cli.editable_byte_limit = None; // Keep ordinary <= 64 MiB routing active
                ws.open_path(bench_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !matches!(ws.tabs.get(ws.active_tab), Some(TabSlot::Hydrating { .. }))
            })
        },
        "1 MiB ordinary tab disk open completion",
    )
    .await?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        match ws.tabs.get(ws.active_tab) {
            Some(TabSlot::Editable(ed))
                if ed.meta.path.as_deref() == Some(bench_path.as_path())
                    && ed.committed_byte_len() >= 1_048_576 =>
            {
                Ok(())
            }
            Some(TabSlot::Editable(ed)) => Err(format!(
                "1 MiB Editable tab mismatch: path={:?} expected={:?} bytes={}",
                ed.meta.path,
                bench_path,
                ed.committed_byte_len()
            )),
            Some(TabSlot::Failed { error, .. }) => {
                Err(format!("1 MiB tab entered Failed state: {error}"))
            }
            Some(TabSlot::Paged(_)) => {
                Err("1 MiB tab unexpectedly routed to Paged instead of Editable".to_string())
            }
            other => Err(format!(
                "1 MiB tab resolved to unexpected slot state: {:?}",
                other.map(TabSlot::title)
            )),
        }
    })?;

    // Place cursor near interior midpoint of the 1 MiB Rope and focus the Editor BEFORE drawing
    // a frame so `window.rendered_frame.dispatch_tree` binds `Undo` to this Editor's FocusId.
    let (bench_doc_id, editor_entity, rev_before) = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(
                cx,
                |ws,
                 cx|
                 -> Result<
                    (
                        DocumentId,
                        Entity<gpui_kit::component::input::EditorState>,
                        Revision,
                    ),
                    String,
                > {
                    let ed = ws
                        .active_editable_mut()
                        .ok_or_else(|| "1 MiB tab is not Editable".to_string())?;
                    let doc_id = ed.doc_id;
                    let editor_entity = ed.editor().clone();
                    let mid = ed
                        .committed_rope()
                        .floor_char_boundary(ed.committed_rope().len() / 2);
                    editor_entity.update(cx, |state, cx| {
                        state.set_selected_range(mid..mid, cx);
                        state.focus(window, cx);
                    });
                    Ok((doc_id, editor_entity, ed.visible_revision))
                },
            )
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;

    let paste_payload = format!("{}\n", "p".repeat(65_536));
    let mut samples_ms = Vec::with_capacity(TYPING_BENCH_OPS);

    // 128 synchronous top-level window updates (ASCII insert, UTF-8 insert, 1-char delete, Undo)
    // with ZERO sleeps inside any timed interval and RecoveryHandle active. Each top-level
    // `window_handle.update` synchronously flushes GPUI effects (`InputEditsEvent` + `EditorAdapter` + `RecoveryCommit`).
    for i in 0..TYPING_BENCH_OPS {
        let t0 = Instant::now();
        window_handle
            .update(cx, |_, window, cx| -> Result<(), String> {
                match i % 4 {
                    0 => {
                        editor_entity.update(cx, |state, cx| {
                            state.replace_text_in_range(None, "a", window, cx);
                        });
                    }
                    1 => {
                        editor_entity.update(cx, |state, cx| {
                            state.replace_text_in_range(None, "中", window, cx);
                        });
                    }
                    2 => {
                        editor_entity.update(cx, |state, cx| {
                            let cur = state.cursor();
                            let prev = state.text().floor_char_boundary(cur.saturating_sub(1));
                            state.set_selected_range(prev..cur, cx);
                            state.replace_text_in_range(None, "", window, cx);
                        });
                    }
                    _ => {
                        workspace.update(cx, |ws, cx| -> Result<(), String> {
                            let ed_ref = ws
                                .active_editable_mut()
                                .ok_or_else(|| "missing editable tab during undo".to_string())?;
                            ed_ref.undo(window, cx);
                            Ok(())
                        })?;
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())??;
        samples_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
    }

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed_after)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing editable tab after typing loop".to_string());
        };
        let rev_delta = ed_after.visible_revision.0.saturating_sub(rev_before.0);
        if rev_delta != TYPING_BENCH_OPS as u64 {
            return Err(format!(
                "expected {} committed revisions in 1 MiB typing loop, got {rev_delta}",
                TYPING_BENCH_OPS
            ));
        }
        Ok(())
    })?;

    // Separate single 64 KiB paste submission timing (exercises >64 KiB checkpoint promotion).
    let t_paste = Instant::now();
    window_handle
        .update(cx, |_, window, cx| {
            editor_entity.update(cx, |state, cx| {
                state.replace_text_in_range(None, &paste_payload, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    let paste_ms = t_paste.elapsed().as_secs_f64() * 1000.0;
    samples_ms.sort_by(|a, b| a.total_cmp(b));
    report.typing_latency_label = TYPING_LATENCY_LABEL.to_string();
    report.typing_doc_bytes = written_bytes as u64;
    report.typing_sample_count = samples_ms.len();
    report.typing_submit_p50_ms = percentile_sorted(&samples_ms, 0.50);
    report.typing_submit_p95_ms = percentile_sorted(&samples_ms, 0.95);
    report.typing_submit_max_ms = samples_ms.last().copied().unwrap_or(0.0);
    report.large_paste_64k_submit_ms = Some(paste_ms);

    // Discard and close the temporary 1 MiB benchmark tab via the durable close barrier.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(ix) = ws.tabs.iter().position(|t| t.doc_id() == bench_doc_id) {
                    ws.close_tab_confirmed(ix, true, window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                !workspace
                    .read(cx)
                    .tabs
                    .iter()
                    .any(|t| t.doc_id() == bench_doc_id)
            })
        },
        "1 MiB benchmark tab durable discard close",
    )
    .await?;

    Ok(())
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((sorted.len() - 1) as f64 * p.clamp(0.0, 1.0)).ceil() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn write_deterministic_1mib_rust_fixture(path: &Path) -> std::io::Result<usize> {
    use std::io::Write as _;
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut total = 0usize;
    let mut line_no = 0usize;
    while total < 1_048_576 {
        let row = format!(
            "pub fn item_{line_no:05}(v: u64) -> u64 {{ v.wrapping_add({line_no}) }} // 性能基准\n"
        );
        writer.write_all(row.as_bytes())?;
        total += row.len();
        line_no += 1;
    }
    writer.flush()?;
    Ok(total)
}

async fn run_search_and_wait(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    needle: &str,
    algorithm: SearchAlgorithm,
    cx: &mut AsyncApp,
) -> Result<usize, String> {
    let needle_owned = needle.to_string();
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.search.visible = true;
                ws.search.query_kind = SearchQueryKind::LiteralStreaming;
                ws.search.algorithm = algorithm;
                let input_entity: Entity<InputState> = ws.search_query_input.clone();
                input_entity.update(cx, |s, cx| {
                    s.set_value(needle_owned.clone(), window, cx);
                });
                ws.run_search_from_inputs(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).search.status,
                    SearchStatus::Done { .. } | SearchStatus::Error(_)
                )
            })
        },
        "streaming search completion",
    )
    .await?;

    cx.update(|cx| {
        let ws = workspace.read(cx);
        match &ws.search.status {
            SearchStatus::Done { total_emitted, .. } => Ok(*total_emitted),
            SearchStatus::Error(e) => Err(format!("search error: {e}")),
            other => Err(format!("unexpected search status: {other:?}")),
        }
    })
}

fn write_deterministic_paged_fixture(path: &Path, lines: usize) -> std::io::Result<usize> {
    use std::io::Write as _;
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut needles = 0usize;
    for i in 0..lines {
        if i % 500 == 250 {
            writeln!(
                writer,
                "2026-11-06T12:00:00Z INFO line={i:06} marker=SMOKE_你好_NEEDLE payload=ok"
            )?;
            needles += 1;
        } else {
            writeln!(
                writer,
                "2026-11-06T12:00:00Z TRACE line={i:06} subsystem=pagerFiller_0123456789abcdef"
            )?;
        }
    }
    writer.flush()?;
    Ok(needles)
}

fn capture_workspace_metrics(ws: &EditorWorkspace, report: &mut SmokeReport) {
    report.process_to_window_ms = ws.startup_metrics.process_started_ms_to_window;
    report.first_interactive_ms = ws.startup_metrics.first_interactive_ms;
    report.recovery_manifest_ms = ws.startup_metrics.recovery_manifest_ms;
    report.final_tab_count = ws.tabs.len();
    report.hydrated_editor_count = ws.hydrated_editor_count();
    report.hydrated_content_bytes = ws.hydrated_content_bytes();
    report.paged_tab_count = ws
        .tabs
        .iter()
        .filter(|s| matches!(s, TabSlot::Paged(_)))
        .count();
    report.shell_persistence_warning = ws.shell_persistence_warning.clone();
}

async fn wait_until(
    timeout: Duration,
    cx: &mut AsyncApp,
    mut predicate: impl FnMut(&mut AsyncApp) -> bool,
    step_name: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate(cx) {
            return Ok(());
        }
        sleep_ms(20, cx).await;
    }
    if predicate(cx) {
        Ok(())
    } else {
        Err(format!("timed out waiting for {step_name}"))
    }
}

async fn sleep_ms(ms: u64, cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(ms))
        .await;
}

fn pump_frame(window_handle: &AnyWindowHandle, cx: &mut AsyncApp) -> Result<(), String> {
    window_handle
        .update(cx, |_, window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        })
        .map_err(|e| format!("pump_frame failed: {e}"))
}

fn dispatch_smoke_key(
    window_handle: &AnyWindowHandle,
    cx: &mut AsyncApp,
    key: &'static str,
) -> Result<(), String> {
    pump_frame(window_handle, cx)?;
    let keystroke = gpui_kit::Keystroke::parse(key).map_err(|e| e.to_string())?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_keystroke(keystroke, cx);
        })
        .map_err(|e| e.to_string())?;
    pump_frame(window_handle, cx)
}

/// Reopens `recovery.sqlite3` AFTER primary `RecoveryHandle::shutdown()` ACKs (and after
/// dropping the primary handle) to verify that:
/// 1. Tombstoned `old_doc_id` (`state = 2`) is completely absent from `SessionManifest`.
/// 2. Reloaded `fresh_doc_id` (`LifecycleGen(1)`) is present in `SessionManifest` and its
///    hydrated draft starts with the post-reload edit.
/// 3. Clean empty untitled `clean_untitled_id` (when provided) is persisted as `!has_checkpoint`
///    with `path == None` and routes to `StubHydrationSource::EmptyCleanUntitled`.
pub fn verify_post_shutdown_manifest(
    data_dir: &Path,
    fresh_doc_id: DocumentId,
    old_doc_id: DocumentId,
    expected_clean_untitled_doc: Option<DocumentId>,
) -> Result<(), String> {
    let db_path = data_dir.join("recovery.sqlite3");
    let handle = RecoveryHandle::start_with_faults(&db_path, FaultInjector::default())
        .map_err(|e| format!("post-shutdown recovery start failed: {e}"))?;
    let check_res = (|| -> Result<(), String> {
        let manifest_rx = handle
            .request_manifest()
            .map_err(|e| format!("post-shutdown request_manifest failed: {e}"))?;
        let manifest = manifest_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("post-shutdown manifest timeout: {e}"))?
            .map_err(|e| format!("post-shutdown manifest error: {e}"))?;

        if manifest.tabs.iter().any(|t| t.doc_id == old_doc_id) {
            return Err(format!(
                "tombstoned old_doc_id {old_doc_id:?} resurrected in post-shutdown SessionManifest"
            ));
        }
        let Some(fresh_stub) = manifest.tabs.iter().find(|t| t.doc_id == fresh_doc_id) else {
            return Err(format!(
                "reloaded fresh_doc_id {fresh_doc_id:?} missing from post-shutdown SessionManifest"
            ));
        };
        if !fresh_stub.has_checkpoint
            || fresh_stub.durable_rev.0 == 0
            || fresh_stub.lifecycle_gen != LifecycleGen(1)
        {
            return Err(format!(
                "unexpected post-shutdown stub state for fresh_doc_id: has_checkpoint={}, durable_rev={:?}, lifecycle_gen={:?}",
                fresh_stub.has_checkpoint, fresh_stub.durable_rev, fresh_stub.lifecycle_gen
            ));
        }
        let draft_rx = handle
            .request_hydrate(fresh_doc_id)
            .map_err(|e| format!("post-shutdown request_hydrate failed: {e}"))?;
        let draft = draft_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("post-shutdown request_hydrate timeout: {e}"))?
            .map_err(|e| format!("post-shutdown request_hydrate error: {e}"))?;
        let prefix: String = draft.rope.chars().take(35).collect();
        if !prefix.starts_with("// post-reload edit on fresh doc_id") {
            return Err(format!(
                "post-shutdown hydrated draft did not preserve post-reload edit (prefix={prefix:?})"
            ));
        }

        if let Some(clean_id) = expected_clean_untitled_doc {
            let Some(clean_stub) = manifest.tabs.iter().find(|t| t.doc_id == clean_id) else {
                return Err(format!(
                    "clean untitled tab {clean_id:?} missing from post-shutdown SessionManifest"
                ));
            };
            let route = classify_stub_hydration_source(
                clean_stub.has_checkpoint,
                clean_stub.meta.path.is_some(),
            );
            if route != StubHydrationSource::EmptyCleanUntitled {
                return Err(format!(
                    "expected EmptyCleanUntitled route for clean untitled stub, got {route:?}"
                ));
            }
        }
        Ok(())
    })();
    let shutdown_res = handle
        .shutdown()
        .map_err(|e| format!("post-shutdown handle.shutdown() failed: {e}"))
        .and_then(|rx| {
            rx.recv_timeout(Duration::from_secs(5))
                .map_err(|e| format!("post-shutdown handle.shutdown() timeout: {e}"))?
                .map_err(|e| format!("post-shutdown handle.shutdown() error: {e}"))
        });
    check_res.and(shutdown_res)
}

async fn run_usability_scenario(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    // 0. Wait for both recovery and shell store bootstrap so prefs/MRU persist cleanly to disk.
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                let active_ready = ws
                    .tabs
                    .get(ws.active_tab)
                    .is_some_and(|s| !matches!(s, TabSlot::Hydrating { .. }));
                ws.recovery_ready && ws.shell_loaded && active_ready
            })
        },
        "recovery and ui_shell bootstrap",
    )
    .await?;

    let raw_work_dir = cx.update(|cx| workspace.read(cx).cli.data_dir.join("usability_artifacts"));
    std::fs::create_dir_all(&raw_work_dir)
        .map_err(|e| format!("create usability_artifacts: {e}"))?;
    let work_dir = std::fs::canonicalize(&raw_work_dir).unwrap_or(raw_work_dir);

    // Verify default sidebar open & Spaces: 2 default.
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if !ws.sidebar_visible() {
            return Err("expected sidebar_visible=true by default".to_string());
        }
        if ws.current_view_prefs() != EditorViewPrefs::default() {
            return Err(format!(
                "expected default EditorViewPrefs (Spaces: 2, soft_wrap: true, show_whitespaces: false), got {:?}",
                ws.current_view_prefs()
            ));
        }
        Ok(())
    })?;

    // =========================================================================
    // 1. Palette Undo & SelectAll Target Focus (no CommandItem.action focus trap)
    // =========================================================================
    let palette_sample = "palette target focus check\nsecond line";
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.create_untitled_tab(true, window, cx);
                let ed = ws
                    .active_editable_mut()
                    .ok_or_else(|| "missing editable tab for palette test".to_string())?;
                let editor_entity = ed.editor().clone();
                editor_entity.update(cx, |state, cx| {
                    state.replace_text_in_range(None, palette_sample, window, cx);
                    state.set_selected_range(0..0, cx);
                });
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;

    // Open Command Palette -> focus moves into palette search input.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_command_palette(window, cx);
                ws.set_palette_query("Edit: Select All", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    pump_frame(window_handle, cx)?;

    // Confirm "Edit: Select All" -> must close palette, focus EditorState, and select entire Editor buffer!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                if !ws.confirm_selected_palette_item(window, cx) {
                    return Err("confirm_selected_palette_item(Select All) returned false".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;

    window_handle
        .update(cx, |_, window, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            if ws.palette_visible {
                return Err("palette remained visible after confirming Select All".into());
            }
            let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
                return Err("active tab is not Editable after Select All".into());
            };
            let state = ed.editor().read(cx);
            if !state.focus_handle(cx).is_focused(window) {
                return Err(
                    "active EditorState did not regain focus after palette Select All".into(),
                );
            }
            if state.selected_range() != (0..palette_sample.len()) {
                return Err(format!(
                    "palette Select All did not select editor text: got {:?}, expected 0..{}",
                    state.selected_range(),
                    palette_sample.len()
                ));
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;

    // Open Command Palette again -> confirm "Edit: Undo" -> must undo EditorState, NOT palette input!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_command_palette(window, cx);
                ws.set_palette_query("Edit: Undo", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    pump_frame(window_handle, cx)?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                if !ws.confirm_selected_palette_item(window, cx) {
                    return Err("confirm_selected_palette_item(Undo) returned false".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;

    window_handle
        .update(cx, |_, window, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
                return Err("active tab is not Editable after palette Undo".into());
            };
            if !ed.editor().read(cx).focus_handle(cx).is_focused(window) {
                return Err("active EditorState did not regain focus after palette Undo".into());
            }
            if ed.committed_byte_len() != 0 || ed.is_dirty() {
                return Err(format!(
                    "palette Undo failed to revert active editor: bytes={} dirty={}",
                    ed.committed_byte_len(),
                    ed.is_dirty()
                ));
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    let (go_to_key, palette_key, find_key, duplicate_key, undo_key) = if cfg!(target_os = "macos") {
        ("cmd-l", "cmd-shift-p", "cmd-f", "cmd-shift-d", "cmd-z")
    } else {
        ("ctrl-g", "ctrl-shift-p", "ctrl-f", "ctrl-shift-d", "ctrl-z")
    };
    dispatch_smoke_key(window_handle, cx, go_to_key)?;
    dispatch_smoke_key(window_handle, cx, palette_key)?;
    dispatch_smoke_key(window_handle, cx, "escape")?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                // The actual keymap dispatch above must restore a mounted editor.
                let editor_focused = matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.editor().read(cx).focus_handle(cx).is_focused(window)
                );
                if !editor_focused {
                    return Err("palette dismissal restored unmounted Go To focus".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    dispatch_smoke_key(window_handle, cx, palette_key)?;
    dispatch_smoke_key(window_handle, cx, find_key)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                if ws.palette_visible
                    || !ws
                        .search_query_input
                        .read(cx)
                        .focus_handle(cx)
                        .is_focused(window)
                {
                    return Err("opening Find left a palette backdrop or incorrect focus".into());
                }
                ws.on_escape(&gpui_kit::component::input::Escape, window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    dispatch_smoke_key(window_handle, cx, "x")?;
    dispatch_smoke_key(window_handle, cx, find_key)?;
    dispatch_smoke_key(window_handle, cx, duplicate_key)?;
    window_handle
        .update(cx, |_, _, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
                return Err("missing editor for shortcut scope check".into());
            };
            if ed.committed_rope().to_string() != "x" {
                return Err("editor-only duplicate shortcut modified document from Find".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    dispatch_smoke_key(window_handle, cx, "escape")?;
    dispatch_smoke_key(window_handle, cx, duplicate_key)?;
    window_handle
        .update(cx, |_, _, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
                return Err("missing editor after duplicate shortcut".into());
            };
            if ed.committed_rope().to_string() != "x\nx" {
                return Err("editor duplicate shortcut did not duplicate the active line".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    dispatch_smoke_key(window_handle, cx, undo_key)?;
    dispatch_smoke_key(window_handle, cx, "alt-z")?;
    window_handle
        .update(cx, |_, _, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
                return Err("missing editor after wrap shortcut".into());
            };
            if ws.current_view_prefs().soft_wrap || ed.committed_rope().to_string() != "x" {
                return Err("Alt+Z failed to toggle wrap or inserted document text".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    dispatch_smoke_key(window_handle, cx, "alt-z")?;
    dispatch_smoke_key(window_handle, cx, undo_key)?;
    report.palette_undo_select_all_focus_verified = true;

    // =========================================================================
    // 2. Settings Non-Dirty & Lazy Inactive Apply + Metadata-Only Switcher/Sidebar
    // =========================================================================
    let lazy_a_path = work_dir.join("lazy_a.rs");
    let active_b_path = work_dir.join("active_b.rs");
    let unhydrated_c_path = work_dir.join("unhydrated_c.rs");
    std::fs::write(&lazy_a_path, "fn lazy_a() {}\n").map_err(|e| e.to_string())?;
    std::fs::write(&active_b_path, "fn active_b() {}\n").map_err(|e| e.to_string())?;
    std::fs::write(&unhydrated_c_path, "fn unhydrated_c() {}\n").map_err(|e| e.to_string())?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_path(lazy_a_path.clone(), window, cx);
                ws.open_path(active_b_path.clone(), window, cx);
                ws.tabs.push(TabSlot::Dehydrated(SessionTabStub {
                    doc_id: DocumentId::new(),
                    lifecycle_gen: LifecycleGen(1),
                    durable_rev: Revision(0),
                    meta: DocMeta {
                        title: "unhydrated_c.rs".to_string(),
                        path: Some(unhydrated_c_path.clone()),
                        ..DocMeta::default()
                    },
                    view: ViewState::default(),
                    has_checkpoint: false,
                }));
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                let a_ready = ws.tabs.iter().any(|s| {
                    matches!(s, TabSlot::Editable(ed) if ed.meta.path.as_deref() == Some(lazy_a_path.as_path()))
                });
                let b_ready = ws.tabs.iter().any(|s| {
                    matches!(s, TabSlot::Editable(ed) if ed.meta.path.as_deref() == Some(active_b_path.as_path()))
                });
                a_ready && b_ready
            })
        },
        "lazy_a and active_b disk open completion",
    )
    .await?;

    // Activate `active_b`, mutate global view prefs + language override, and open Quick Switcher to filter.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let b_ix = ws
                    .tabs
                    .iter()
                    .position(|s| s.meta().path.as_deref() == Some(active_b_path.as_path()))
                    .ok_or_else(|| "missing active_b tab".to_string())?;
                ws.activate_tab(b_ix, window, cx);
                ws.set_soft_wrap(false, window, cx);
                ws.set_show_whitespaces(true, window, cx);
                ws.set_indent_prefs(4, false, window, cx);
                ws.set_active_language_override(Some("python"), window, cx);

                // Open Quick Switcher (Cmd+P) and filter over unhydrated tab + `>` command mode:
                ws.open_quick_switcher(window, cx);
                ws.set_palette_query("unhydrated_c", window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;

    // Exercise `>` command prefix inside Quick Switcher and dismiss, verifying zero hydration of `unhydrated_c`!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.set_palette_query("> Toggle Soft Wrap", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    pump_frame(window_handle, cx)?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.dismiss_palette(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    let expected_updated_prefs = EditorViewPrefs {
        soft_wrap: false,
        show_whitespaces: true,
        tab_size: 4,
        hard_tabs: false,
    };

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed_b)) = ws
            .tabs
            .iter()
            .find(|s| s.meta().path.as_deref() == Some(active_b_path.as_path()))
        else {
            return Err("active_b missing".into());
        };
        if ed_b.is_dirty() || ed_b.visible_revision != Revision(0) {
            return Err("view prefs or language_override falsely dirtied active_b!".into());
        }
        if ed_b.applied_prefs() != expected_updated_prefs || ed_b.language.id != "python" {
            return Err(format!(
                "active_b did not apply prefs/language immediately: prefs={:?} lang={}",
                ed_b.applied_prefs(),
                ed_b.language.id
            ));
        }

        let Some(TabSlot::Editable(ed_a)) = ws
            .tabs
            .iter()
            .find(|s| s.meta().path.as_deref() == Some(lazy_a_path.as_path()))
        else {
            return Err("lazy_a missing".into());
        };
        if ed_a.applied_prefs() != EditorViewPrefs::default() {
            return Err(format!(
                "inactive lazy_a eagerly applied prefs instead of waiting for activation: {:?}",
                ed_a.applied_prefs()
            ));
        }

        let Some(slot_c) = ws
            .tabs
            .iter()
            .find(|s| s.meta().path.as_deref() == Some(unhydrated_c_path.as_path()))
        else {
            return Err("unhydrated_c missing".into());
        };
        if !matches!(slot_c, TabSlot::Dehydrated(_)) {
            return Err(
                "filtering Quick Switcher / Sidebar unexpectedly hydrated unhydrated_c!".into(),
            );
        }
        Ok(())
    })?;
    report.switcher_metadata_only_verified = true;

    // Activate `lazy_a` now -> must lazily receive `expected_updated_prefs` without becoming dirty!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let a_ix = ws
                    .tabs
                    .iter()
                    .position(|s| s.meta().path.as_deref() == Some(lazy_a_path.as_path()))
                    .ok_or_else(|| "missing lazy_a index".to_string())?;
                ws.activate_tab(a_ix, window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed_a)) = ws.tabs.get(ws.active_tab) else {
            return Err("lazy_a not Editable on activation".into());
        };
        if ed_a.applied_prefs() != expected_updated_prefs || ed_a.is_dirty() {
            return Err(format!(
                "lazy_a activation prefs mismatch: prefs={:?} dirty={}",
                ed_a.applied_prefs(),
                ed_a.is_dirty()
            ));
        }
        Ok(())
    })?;
    report.settings_nondirty_and_lazy_apply_verified = true;

    // =========================================================================
    // 3. CRLF Line Ops + Single-Step Undo + JSON Comment Capability Gate
    // =========================================================================
    let crlf_path = work_dir.join("crlf_ops.rs");
    let crlf_initial = "fn crlf_demo() {\r\n    let n = 1;\r\n}";
    std::fs::write(&crlf_path, crlf_initial.as_bytes()).map_err(|e| e.to_string())?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_path(crlf_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.meta.path.as_deref() == Some(crlf_path.as_path())
                )
            })
        },
        "crlf_ops.rs open completion",
    )
    .await?;

    pump_frame(window_handle, cx)?;

    // Select lines 2..=3 ("    let n = 1;\r\n}") and run ToggleComment, Duplicate, MoveUp/MoveDown.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws.active_editable_mut().ok_or("missing crlf_ops tab")?;
                if ed.meta.saved_encoding != Some(EncodingSpec::Utf8)
                    || ed.meta.saved_line_ending != Some(LineEnding::Crlf)
                    || ed.is_dirty()
                {
                    return Err(format!(
                        "fresh disk open baseline mismatch: saved_enc={:?} saved_eol={:?} dirty={}",
                        ed.meta.saved_encoding,
                        ed.meta.saved_line_ending,
                        ed.is_dirty()
                    ));
                }
                let len = ed.committed_rope().len();
                ed.editor().update(cx, |state, cx| {
                    state.set_selected_range(18..len, cx);
                    state.focus(window, cx);
                });
                ws.on_toggle_comment(&ToggleComment, window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing crlf_ops tab".into());
        };
        let text = ed.committed_rope().to_string();
        if text != "fn crlf_demo() {\r\n    // let n = 1;\r\n// }"
            || ed.visible_revision != Revision(1)
        {
            return Err(format!(
                "CRLF ToggleComment mismatch: rev={} text={text:?}",
                ed.visible_revision.0
            ));
        }
        Ok(())
    })?;

    // Single Undo MUST revert the entire multi-line CRLF ToggleComment batch!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws.active_editable_mut().ok_or("missing crlf_ops tab")?;
                ed.undo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.tabs.get(ws.active_tab) else {
            return Err("missing crlf_ops tab after undo".into());
        };
        if ed.committed_rope().to_string() != crlf_initial {
            return Err("single Undo did not revert full CRLF ToggleComment batch!".into());
        }
        Ok(())
    })?;

    // Duplicate + MoveUp/MoveDown at CRLF EOF without trailing newline + JSON comment gate:
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let ed = ws.active_editable_mut().ok_or("missing crlf_ops tab")?;
                let eof = ed.committed_rope().len();
                ed.editor().update(cx, |state, cx| {
                    state.set_selected_range(eof..eof, cx);
                });
                ws.on_duplicate_line(&DuplicateLine, window, cx);
                ws.on_move_line_up(&MoveLineUp, window, cx);
                ws.on_move_line_down(&MoveLineDown, window, cx);

                // Undo x3 back to original, then switch to JSON and verify ToggleComment is gated off:
                let ed = ws.active_editable_mut().unwrap();
                ed.undo(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(ed) = ws.active_editable_mut() {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(ed) = ws.active_editable_mut() {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.set_active_language_override(Some("json"), window, cx);
                let rev_before = ws
                    .active_editable_mut()
                    .ok_or("missing tab")?
                    .visible_revision;
                ws.on_toggle_comment(&ToggleComment, window, cx);
                let ed = ws.active_editable_mut().unwrap();
                if ed.visible_revision != rev_before
                    || ed.committed_rope().to_string() != crlf_initial
                {
                    return Err("ToggleComment mutated buffer while language was JSON!".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    report.crlf_line_ops_single_undo_verified = true;

    // =========================================================================
    // 4. Empty Untouched Untitled Prefs Persist via Layout Without False Dirty
    // =========================================================================
    let empty_untitled_id = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<DocumentId, String> {
                let id = ws.create_untitled_tab(true, window, cx);
                ws.set_active_line_ending(LineEnding::Crlf, window, cx);
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                ws.set_active_language_override(Some("rust"), window, cx);
                let ed = ws.active_editable_mut().ok_or("missing untitled")?;
                if ed.is_dirty()
                    || ed.visible_revision != Revision(0)
                    || ed.durable_revision != Revision(0)
                {
                    return Err(format!(
                        "0-byte untitled format/language customization falsely dirtied or advanced rev: dirty={} vis={} dur={}",
                        ed.is_dirty(),
                        ed.visible_revision.0,
                        ed.durable_revision.0
                    ));
                }
                Ok(id)
            })
        })
        .map_err(|e| e.to_string())??;
    report.expected_usability_empty_untitled_doc = Some(empty_untitled_id);
    report.empty_untitled_prefs_layout_verified = true;

    // =========================================================================
    // 5. Format Identity Reselect + Flip-Back + Save-In-Flight Format Change
    // =========================================================================
    let inflight_path = work_dir.join("format_inflight.txt");
    std::fs::write(&inflight_path, "alpha\n").map_err(|e| e.to_string())?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_path(inflight_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.meta.path.as_deref() == Some(inflight_path.as_path())
                )
            })
        },
        "format_inflight.txt open",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                // Identity re-selection -> strict NoOp
                ws.set_active_line_ending(LineEnding::Lf, window, cx);
                ws.set_active_encoding(EncodingSpec::Utf8, window, cx);
                {
                    let ed = ws.active_editable_mut().unwrap();
                    if ed.visible_revision != Revision(0) || ed.is_dirty() {
                        return Err("identity format re-selection was not a NoOp!".into());
                    }
                }

                // Switch to BOM -> dirty rev 1; flip back to Utf8 -> clean rev 2!
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                {
                    let ed = ws.active_editable_mut().unwrap();
                    if !ed.is_dirty() || ed.visible_revision != Revision(1) {
                        return Err("switching to Utf8Bom did not mark dirty at rev 1".into());
                    }
                }
                ws.set_active_encoding(EncodingSpec::Utf8, window, cx);
                {
                    let ed = ws.active_editable_mut().unwrap();
                    if ed.is_dirty() || ed.visible_revision != Revision(2) {
                        return Err(format!(
                            "flipping back to Utf8 did not clear dirty: dirty={} rev={}",
                            ed.is_dirty(),
                            ed.visible_revision.0
                        ));
                    }
                }

                // Type edit, kick off async Save (captured Utf8), and IMMEDIATELY switch to Utf8Bom while save is in flight!
                let editor_entity = ws.active_editable_mut().unwrap().editor().clone();
                editor_entity.update(cx, |state, cx| {
                    state.replace_text_in_range(Some(6..6), "beta\n", window, cx);
                });
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    let captured_save_rev = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Revision {
                let rev_for_save = ws.active_editable_mut().unwrap().visible_revision;
                ws.save_tab(ws.active_tab, None, false, false, window, cx);
                // Mutate format WHILE background save is in flight:
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                rev_for_save
            })
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.meta.saved_identity.is_some_and(|id| id.byte_len == "alpha\nbeta\n".len() as u64)
                            && ed.visible_revision > captured_save_rev
                )
            })
        },
        "in-flight save completion during concurrent format switch",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                {
                    let ed = ws.active_editable_mut().unwrap();
                    if ed.meta.saved_encoding != Some(EncodingSpec::Utf8)
                        || ed.meta.encoding != EncodingSpec::Utf8Bom
                        || !ed.is_dirty()
                    {
                        return Err(format!(
                            "format change during in-flight save failed to remain dirty: saved_enc={:?} cur_enc={:?} dirty={}",
                            ed.meta.saved_encoding,
                            ed.meta.encoding,
                            ed.is_dirty()
                        ));
                    }
                }
                // Flip Utf8Bom back to saved Utf8 -> must become clean immediately!
                ws.set_active_encoding(EncodingSpec::Utf8, window, cx);
                let ed = ws.active_editable_mut().unwrap();
                if ed.is_dirty() {
                    return Err("flipping format back after in-flight save did not clear dirty!".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    report.format_flip_back_and_inflight_save_verified = true;

    // =========================================================================
    // 6. Format-Only BOM Dirty Restart -> Still Dirty -> Save Writes Actual EF BB BF
    // =========================================================================
    let bom_restart_path = work_dir.join("bom_restart_save.txt");
    std::fs::write(&bom_restart_path, "bom payload\n").map_err(|e| e.to_string())?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_path(bom_restart_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.meta.path.as_deref() == Some(bom_restart_path.as_path())
                )
            })
        },
        "bom_restart_save.txt open",
    )
    .await?;

    let bom_restart_doc_id = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> DocumentId {
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                ws.tabs[ws.active_tab].doc_id()
            })
        })
        .map_err(|e| e.to_string())?;

    // Wait for durable SQLite checkpoint of the format-only BOM change (zero text edits!).
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed))
                        if ed.doc_id == bom_restart_doc_id
                            && ed.is_dirty()
                            && ed.durable_revision >= ed.visible_revision
                            && ed.durable_revision.0 > 0
                )
            })
        },
        "durable recovery checkpoint of format-only BOM change",
    )
    .await?;

    // Fetch real `SessionManifest` stub from `RecoveryHandle` and re-hydrate via `RecoveryDatabase`
    // exactly as a fresh restart does!
    let recovery_clone = cx
        .update(|cx| workspace.read(cx).recovery.clone())
        .ok_or_else(|| "missing recovery handle".to_string())?;
    let persisted_stub = cx
        .background_executor()
        .spawn(async move {
            let manifest = recovery_clone
                .request_manifest()
                .map_err(|e| e.to_string())?
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            manifest
                .tabs
                .into_iter()
                .find(|t| t.doc_id == bom_restart_doc_id)
                .ok_or_else(|| "bom_restart_doc_id missing from SQLite SessionManifest".to_string())
        })
        .await?;

    if !persisted_stub.has_checkpoint {
        return Err("format-only BOM change did not persist has_checkpoint=true in SQLite".into());
    }

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                let ix = ws.active_tab;
                ws.tabs[ix] = TabSlot::Dehydrated(persisted_stub);
                ws.activate_tab(ix, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.doc_id == bom_restart_doc_id
                )
            })
        },
        "restart hydration of format-only BOM draft",
    )
    .await?;

    // Verify hydrated draft is STILL dirty before saving, then Save and assert EF BB BF on disk!
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                {
                    let ed = ws.active_editable_mut().ok_or("missing rehydrated bom tab")?;
                    if !ed.is_dirty()
                        || ed.meta.encoding != EncodingSpec::Utf8Bom
                        || ed.meta.saved_encoding != Some(EncodingSpec::Utf8)
                    {
                        return Err(format!(
                            "rehydrated format-only BOM draft lost dirty state: dirty={} enc={:?} saved_enc={:?}",
                            ed.is_dirty(),
                            ed.meta.encoding,
                            ed.meta.saved_encoding
                        ));
                    }
                }
                ws.on_save_file(&SaveFile, window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.doc_id == bom_restart_doc_id && !ed.is_dirty()
                )
            })
        },
        "save completion of rehydrated BOM tab",
    )
    .await?;

    let saved_bom_bytes = std::fs::read(&bom_restart_path).map_err(|e| e.to_string())?;
    if !saved_bom_bytes.starts_with(&[0xEF, 0xBB, 0xBF])
        || &saved_bom_bytes[3..] != b"bom payload\n"
    {
        return Err(format!(
            "saved BOM file bytes mismatch: got {:02X?}",
            &saved_bom_bytes[..saved_bom_bytes.len().min(8)]
        ));
    }

    // Leave one format-only BOM dirty file open across process shutdown for `verify_post_shutdown_usability`!
    let post_shutdown_bom_path = work_dir.join("bom_post_shutdown.txt");
    std::fs::write(&post_shutdown_bom_path, "post shutdown bom\n").map_err(|e| e.to_string())?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_path(post_shutdown_bom_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.tabs.get(ws.active_tab),
                    Some(TabSlot::Editable(ed)) if ed.meta.path.as_deref() == Some(post_shutdown_bom_path.as_path())
                )
            })
        },
        "bom_post_shutdown.txt open",
    )
    .await?;

    let post_shutdown_bom_doc = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> DocumentId {
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                ws.tabs[ws.active_tab].doc_id()
            })
        })
        .map_err(|e| e.to_string())?;

    report.expected_usability_bom_dirty_doc = Some(post_shutdown_bom_doc);
    report.expected_usability_mru_paths = vec![
        post_shutdown_bom_path,
        bom_restart_path,
        inflight_path,
        crlf_path,
    ];
    report.format_only_bom_restart_and_save_verified = true;
    report.mru_persistence_verified = true;

    pump_frame(window_handle, cx)?;
    cx.update(|cx| {
        capture_workspace_metrics(workspace.read(cx), report);
    });
    Ok(())
}

/// Post-shutdown SQLite + `ui_shell.json` verification for `--smoke=usability`.
pub fn verify_post_shutdown_usability(data_dir: &Path, report: &SmokeReport) -> Result<(), String> {
    if let Some(shell_warn) = &report.shell_persistence_warning {
        return Err(format!(
            "unexpected shell_persistence_warning: {shell_warn}"
        ));
    }

    // 1. Verify `ui_shell.json` preferences and MRU history via `UiShellStore::open`.
    let shell_path = data_dir.join("ui_shell.json");
    let (shell_store, shell_load) = UiShellStore::open(&shell_path)
        .map_err(|e| format!("post-shutdown UiShellStore::open failed: {e}"))?;
    if let Some(warn) = shell_load.warning {
        return Err(format!("post-shutdown ui_shell.json warning: {warn}"));
    }
    if shell_load.state.soft_wrap
        || !shell_load.state.show_whitespaces
        || shell_load.state.tab_size != 4
        || shell_load.state.hard_tabs
    {
        return Err(format!(
            "post-shutdown ui_shell.json prefs mismatch: {:?}",
            shell_load.state
        ));
    }
    for expected_path in &report.expected_usability_mru_paths {
        if !shell_load.state.recent_files.contains(expected_path) {
            return Err(format!(
                "expected path {} missing from persisted recent_files {:?}",
                expected_path.display(),
                shell_load.state.recent_files
            ));
        }
    }
    if let Some(most_recent) = report.expected_usability_mru_paths.first()
        && shell_load.state.recent_files.first() != Some(most_recent)
    {
        return Err(format!(
            "MRU head mismatch: got {:?}, expected {:?}",
            shell_load.state.recent_files.first(),
            most_recent
        ));
    }
    shell_store
        .shutdown()
        .map_err(|e| format!("post-shutdown shell_store.shutdown failed: {e}"))?
        .recv_timeout(Duration::from_secs(3))
        .map_err(|e| format!("post-shutdown shell_store.shutdown timeout: {e}"))?
        .map_err(|e| format!("post-shutdown shell_store.shutdown error: {e}"))?;

    // 2. Verify SQLite `SessionManifest` for 0-byte untitled layout prefs AND format-only BOM dirty draft!
    let db_path = data_dir.join("recovery.sqlite3");
    let handle = RecoveryHandle::start_with_faults(&db_path, FaultInjector::default())
        .map_err(|e| format!("post-shutdown usability recovery start failed: {e}"))?;

    let check_res = (|| -> Result<(), String> {
        let manifest = handle
            .request_manifest()
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;

        if let Some(untitled_id) = report.expected_usability_empty_untitled_doc {
            let stub = manifest
                .tabs
                .iter()
                .find(|t| t.doc_id == untitled_id)
                .ok_or_else(|| {
                    "0-byte untitled tab missing from post-shutdown manifest".to_string()
                })?;
            if stub.has_checkpoint
                || stub.meta.line_ending != LineEnding::Crlf
                || stub.meta.encoding != EncodingSpec::Utf8Bom
                || stub.meta.language_override.as_deref() != Some("rust")
            {
                return Err(format!(
                    "0-byte untitled post-shutdown stub mismatch: has_checkpoint={} eol={:?} enc={:?} lang={:?}",
                    stub.has_checkpoint,
                    stub.meta.line_ending,
                    stub.meta.encoding,
                    stub.meta.language_override
                ));
            }
        }

        if let Some(bom_id) = report.expected_usability_bom_dirty_doc {
            let stub = manifest
                .tabs
                .iter()
                .find(|t| t.doc_id == bom_id)
                .ok_or_else(|| {
                    "format-only BOM dirty tab missing from post-shutdown manifest".to_string()
                })?;
            if !stub.has_checkpoint {
                return Err(
                    "format-only BOM dirty tab missing has_checkpoint=true after shutdown".into(),
                );
            }
            let draft = handle
                .request_hydrate(bom_id)
                .map_err(|e| e.to_string())?
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            if draft.meta.encoding != EncodingSpec::Utf8Bom
                || draft.meta.saved_encoding != Some(EncodingSpec::Utf8)
                || !compute_metadata_dirty(&draft.meta)
            {
                return Err(format!(
                    "hydrated post-shutdown BOM draft lost metadata dirty baseline: enc={:?} saved_enc={:?}",
                    draft.meta.encoding, draft.meta.saved_encoding
                ));
            }
        }
        Ok(())
    })();

    let shutdown_res = handle.shutdown().map_err(|e| e.to_string()).and_then(|rx| {
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    });
    check_res.and(shutdown_res)
}
