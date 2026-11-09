use super::*;
use crate::actions::CloseActiveTab;
use editor_services::{ContentIdentity, HydratedDraft};
use gpui_kit::component::input::{EditorState, InputEditsEvent, InputEvent, Redo, Undo};
use gpui_kit::{AppContext as _, Context, Subscription};
use ropey::Rope;
use ropey::extra::esoterica::ropes_are_instances;
use std::io::Write as _;

const IME_BENCH_CYCLES: usize = 32;
const IME_BENCH_OPS_PER_CYCLE: usize = 6;
const IME_BENCH_TOTAL_SAMPLES: usize = IME_BENCH_CYCLES * IME_BENCH_OPS_PER_CYCLE;
const IME_BENCH_EXPECTED_REV_DELTA: u64 = (IME_BENCH_CYCLES * 2) as u64;
const NEAR_64MIB_ORDINARY_BYTES: usize = 64 * 1024 * 1024 - 4; // 67,108,860 B (+3 B "你" <= 64 MiB cap)
const BENCH_ROW_64B: &[u8; 64] =
    b"ordinary-ime-benchmark-plaintext-line-0123456789abcdef-row-end!\n";

const OP_NAMES: [&str; IME_BENCH_OPS_PER_CYCLE] = [
    "mark_n",
    "mark_ni",
    "commit_chinese",
    "undo",
    "mark_x",
    "empty_cancel",
];

#[derive(Clone, Debug, Serialize)]
pub struct ImeOpQuantiles {
    pub operation: &'static str,
    pub sample_count: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImeDatasetBenchmark {
    pub dataset: &'static str,
    pub latency_label: &'static str,
    pub exact_doc_bytes: u64,
    pub ordinary_admitted: bool,
    pub cycles: usize,
    pub total_samples: usize,
    pub committed_revision_delta: u64,
    pub final_bytes_unchanged: bool,
    pub durable_drained: bool,
    pub ops: Vec<ImeOpQuantiles>,
}

#[derive(Clone, Debug)]
pub(crate) struct ImePostShutdownExpectation {
    pub expected_layout: SessionLayout,
    pub tombstoned_doc_id: DocumentId,
    pub dirty_unsaved_key: DocumentKey,
    pub dirty_unsaved_rev: Revision,
    pub dirty_unsaved_identity: ContentIdentity,
    pub dirty_unsaved_disk_path: PathBuf,
    pub saved_format_doc_id: DocumentId,
    pub saved_format_identity: ContentIdentity,
    pub saved_format_path: PathBuf,
    pub unmark_saved_doc_id: DocumentId,
    pub unmark_saved_identity: ContentIdentity,
    pub unmark_saved_path: PathBuf,
    pub cleanup_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct ObservedEditStep {
    start_byte: usize,
    old_end_byte: usize,
    new_end_byte: usize,
    post_text_bounded: Option<String>,
}

struct ImeEventObserver {
    counter_only: bool,
    edits_event_count: usize,
    edits_step_count: usize,
    change_event_count: usize,
    batches: Vec<Vec<ObservedEditStep>>,
    _subscriptions: Vec<Subscription>,
}

impl ImeEventObserver {
    fn for_editor(
        editor: &Entity<EditorState>,
        counter_only: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let edits_sub = cx.subscribe(
            editor,
            move |this: &mut Self, _, event: &InputEditsEvent, _cx| {
                this.record_edits(event);
            },
        );
        let input_sub = cx.subscribe(
            editor,
            move |this: &mut Self, _, event: &InputEvent, _cx| {
                if matches!(event, InputEvent::Change) {
                    this.change_event_count += 1;
                }
            },
        );
        Self {
            counter_only,
            edits_event_count: 0,
            edits_step_count: 0,
            change_event_count: 0,
            batches: Vec::new(),
            _subscriptions: vec![edits_sub, input_sub],
        }
    }

    fn for_input(input: &Entity<InputState>, cx: &mut Context<Self>) -> Self {
        let edits_sub = cx.subscribe(
            input,
            move |this: &mut Self, _, event: &InputEditsEvent, _cx| {
                this.record_edits(event);
            },
        );
        let input_sub = cx.subscribe(input, move |this: &mut Self, _, event: &InputEvent, _cx| {
            if matches!(event, InputEvent::Change) {
                this.change_event_count += 1;
            }
        });
        Self {
            counter_only: false,
            edits_event_count: 0,
            edits_step_count: 0,
            change_event_count: 0,
            batches: Vec::new(),
            _subscriptions: vec![edits_sub, input_sub],
        }
    }

    fn record_edits(&mut self, event: &InputEditsEvent) {
        self.edits_event_count += 1;
        self.edits_step_count += event.edits().len();
        if self.counter_only {
            return;
        }
        let steps = event
            .edits()
            .iter()
            .map(|(edit, post)| ObservedEditStep {
                start_byte: edit.start_byte,
                old_end_byte: edit.old_end_byte,
                new_end_byte: edit.new_end_byte,
                post_text_bounded: (post.len() <= 512).then(|| post.to_string()),
            })
            .collect();
        self.batches.push(steps);
    }

    fn take_snapshot(&mut self) -> (usize, usize, Vec<Vec<ObservedEditStep>>) {
        let edits = self.edits_event_count;
        let changes = self.change_event_count;
        let batches = std::mem::take(&mut self.batches);
        self.edits_event_count = 0;
        self.edits_step_count = 0;
        self.change_event_count = 0;
        (edits, changes, batches)
    }
}

fn assert_no_events(
    observer: &Entity<ImeEventObserver>,
    label: &str,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let (edits, changes, batches) = cx.update(|cx| observer.update(cx, |o, _| o.take_snapshot()));
    if edits != 0 || changes != 0 || !batches.is_empty() {
        return Err(format!(
            "{label}: expected 0 InputEditsEvent and 0 InputEvent::Change, got edits={edits} changes={changes} batches={batches:?}"
        ));
    }
    Ok(())
}

fn assert_single_final_event(
    observer: &Entity<ImeEventObserver>,
    expected_final: &str,
    forbidden_intermediates: &[&str],
    label: &str,
    cx: &mut AsyncApp,
) -> Result<ObservedEditStep, String> {
    let (edits, changes, batches) = cx.update(|cx| observer.update(cx, |o, _| o.take_snapshot()));
    if edits != 1 || changes != 1 || batches.len() != 1 {
        return Err(format!(
            "{label}: expected 1 InputEditsEvent and 1 InputEvent::Change, got edits={edits} changes={changes} batches={batches:?}"
        ));
    }
    let steps = &batches[0];
    if steps.len() != 1 {
        return Err(format!(
            "{label}: expected single net edit step inside InputEditsEvent, got {steps:?}"
        ));
    }
    for step in steps {
        if let Some(text) = step.post_text_bounded.as_deref() {
            for forbidden in forbidden_intermediates {
                if text == *forbidden {
                    return Err(format!(
                        "{label}: intermediate preedit snapshot {forbidden:?} leaked into InputEditsEvent: {steps:?}"
                    ));
                }
            }
        }
    }
    let final_step = steps[0].clone();
    if final_step.post_text_bounded.as_deref() != Some(expected_final) {
        return Err(format!(
            "{label}: expected final event snapshot {expected_final:?}, got {:?}",
            final_step.post_text_bounded
        ));
    }
    Ok(final_step)
}

async fn open_owned_editable(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    path: &Path,
    label: &str,
    cx: &mut AsyncApp,
) -> Result<(ViewTarget, Entity<EditorState>), String> {
    let target_path = path.to_path_buf();
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.cli.editable_byte_limit = None;
                ws.open_path(target_path.clone(), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(20),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy()
                    && !ws.layout_dirty
                    && matches!(
                        ws.active_slot(),
                        Some(TabSlot::Editable(ed))
                            if ed.meta.path.as_deref() == Some(path)
                                && !ed.has_pending_native_edit(cx)
                    )
            })
        },
        label,
    )
    .await?;

    cx.update(|cx| {
        let ws = workspace.read(cx);
        let view_id = ws.active_view_id().ok_or("missing active view_id")?;
        let target = ws.view_target(view_id).ok_or("missing active ViewTarget")?;
        let Some(TabSlot::Editable(ed)) = ws.active_slot() else {
            return Err(format!("{label}: active slot is not Editable"));
        };
        Ok((target, ed.editor().clone()))
    })
}

fn focus_editor_at_eof_and_draw(
    window: &AnyWindowHandle,
    editor: &Entity<EditorState>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    window
        .update(cx, |_, window, cx| {
            editor.update(cx, |state, cx| {
                let end = state.text().len();
                state.set_selected_range(end..end, cx);
                state.focus(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    pump_frame(window, cx)
}

fn focus_editor_and_draw(
    window: &AnyWindowHandle,
    editor: &Entity<EditorState>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    window
        .update(cx, |_, window, cx| {
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    pump_frame(window, cx)
}

fn mark_editor_text(
    window: &AnyWindowHandle,
    editor: &Entity<EditorState>,
    text: &'static str,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    focus_editor_and_draw(window, editor, cx)?;
    window
        .update(cx, |_, window, cx| {
            editor.update(cx, |state, cx| {
                let end = text.encode_utf16().count();
                state.replace_and_mark_text_in_range(None, text, Some(end..end), window, cx);
            });
        })
        .map_err(|e| e.to_string())
}

async fn query_manifest_stub(
    workspace: &Entity<EditorWorkspace>,
    doc_id: DocumentId,
    cx: &mut AsyncApp,
) -> Result<SessionDocumentStub, String> {
    let recovery = cx
        .update(|cx| workspace.read(cx).recovery.clone())
        .ok_or("missing RecoveryHandle")?;
    cx.background_executor()
        .spawn(async move {
            let manifest = recovery
                .request_manifest()
                .map_err(|e| e.to_string())?
                .recv_timeout(Duration::from_secs(5))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            manifest
                .documents
                .into_iter()
                .find(|s| s.doc_id == doc_id)
                .ok_or_else(|| format!("doc {doc_id:?} missing from SessionManifest"))
        })
        .await
}

async fn hydrate_durable_draft(
    workspace: &Entity<EditorWorkspace>,
    key: DocumentKey,
    cx: &mut AsyncApp,
) -> Result<HydratedDraft, String> {
    let recovery = cx
        .update(|cx| workspace.read(cx).recovery.clone())
        .ok_or("missing RecoveryHandle")?;
    cx.background_executor()
        .spawn(async move {
            recovery
                .request_hydrate(key, CancelToken::new())
                .map_err(|e| e.to_string())?
                .recv_timeout(Duration::from_secs(10))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        })
        .await
}

fn write_deterministic_plaintext_fixture(path: &Path, exact_bytes: usize) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut block = Vec::with_capacity(64 * 1024);
    for _ in 0..1024 {
        block.extend_from_slice(BENCH_ROW_64B);
    }
    let mut remaining = exact_bytes;
    while remaining >= block.len() {
        writer.write_all(&block)?;
        remaining -= block.len();
    }
    while remaining >= BENCH_ROW_64B.len() {
        writer.write_all(BENCH_ROW_64B)?;
        remaining -= BENCH_ROW_64B.len();
    }
    if remaining > 0 {
        if remaining == 1 {
            writer.write_all(b"\n")?;
        } else {
            writer.write_all(&BENCH_ROW_64B[..remaining - 1])?;
            writer.write_all(b"\n")?;
        }
    }
    writer.flush()
}

pub(super) async fn run_ime_scenario(
    window_handle: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    run_startup_scenario(window_handle, workspace, report, cx).await?;

    let raw_work_dir = cx.update(|cx| workspace.read(cx).cli.data_dir.join("smoke_ime_artifacts"));
    std::fs::create_dir(&raw_work_dir)
        .map_err(|e| format!("create new owned smoke_ime_artifacts directory: {e}"))?;
    let work_dir = std::fs::canonicalize(&raw_work_dir)
        .map_err(|e| format!("resolve owned smoke_ime_artifacts directory: {e}"))?;

    // =========================================================================
    // Phase 1: Zero-state first baseline (durable_revision == 0)
    // Preedit isolation, Save/Close Busy guards, Search & Recovery isolation,
    // net no-op empty cancel, and explicit n -> ni -> 你 commit.
    // =========================================================================
    let baseline_path = work_dir.join("baseline_rev0.txt");
    std::fs::write(&baseline_path, b"a").map_err(|e| format!("write baseline_rev0.txt: {e}"))?;
    let (baseline_target, baseline_editor) = open_owned_editable(
        window_handle,
        workspace,
        &baseline_path,
        "open baseline_rev0.txt",
        cx,
    )
    .await?;

    focus_editor_at_eof_and_draw(window_handle, &baseline_editor, cx)?;
    let observer =
        cx.update(|cx| cx.new(|cx| ImeEventObserver::for_editor(&baseline_editor, false, cx)));

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) else {
            return Err("baseline_rev0 not Editable".into());
        };
        if ed.visible_revision != Revision(0)
            || ed.queued_revision != Revision(0)
            || ed.durable_revision != Revision(0)
            || ed.has_checkpoint
            || ed.is_dirty()
            || ed.has_pending_native_edit(cx)
        {
            return Err("baseline_rev0 did not start clean at durable_revision 0".into());
        }
        Ok(())
    })?;

    // 1A. Mark "n" -> verify Save/Close Busy, Search isolation, Recovery non-durability, then empty cancel.
    mark_editor_text(window_handle, &baseline_editor, "n", cx)?;
    assert_no_events(&observer, "Phase 1A mark_n", cx)?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) else {
                    return Err("baseline_rev0 missing during mark_n".into());
                };
                if ed.editor().read(cx).text().to_string() != "an"
                    || ed.committed_rope().to_string() != "a"
                    || !ed.has_active_ime(window, cx)
                    || !ed.has_pending_native_edit(cx)
                    || ed.visible_revision != Revision(0)
                    || ed.queued_revision != Revision(0)
                    || ed.durable_revision != Revision(0)
                    || ed.is_dirty()
                {
                    return Err("mark_n failed preedit isolation invariants".into());
                }

                let prog_err = ws.apply_active_atomic_edits(
                    Revision(0),
                    &[(0..1, "A".to_string())],
                    window,
                    cx,
                );
                if prog_err != Err(ApplyEditsError::CompositionActive) {
                    return Err(format!(
                        "expected CompositionActive during preedit, got {prog_err:?}"
                    ));
                }

                // Active-preedit SaveFile must reject with Busy without writing preedit or losing mark.
                ws.on_save_file(&SaveFile, window, cx);
                let Some(TabSlot::Editable(ed_after_save)) = ws.view_slot(baseline_target.view_id)
                else {
                    return Err("baseline_rev0 lost after SaveFile attempt".into());
                };
                if !ed_after_save.has_active_ime(window, cx)
                    || ed_after_save.committed_rope().to_string() != "a"
                {
                    return Err("SaveFile during active preedit altered mark or committed text".into());
                }

                // Active-preedit Close must reject with Busy: no lifecycle, no readonly freeze, mark kept.
                ws.request_close_view(baseline_target, window, cx);
                let Some(TabSlot::Editable(ed_after_close)) =
                    ws.view_slot(baseline_target.view_id)
                else {
                    return Err("baseline_rev0 closed during active preedit".into());
                };
                if ws.is_lifecycle_busy()
                    || ws.pending_lifecycle_sequence().is_some()
                    || ed_after_close.editor().read(cx).presentation().is_readonly()
                    || !ed_after_close.has_active_ime(window, cx)
                {
                    return Err(
                        "request_close_view during active preedit entered lifecycle/readonly or lost mark"
                            .into(),
                    );
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    let disk_during_preedit =
        std::fs::read(&baseline_path).map_err(|e| format!("read baseline_rev0.txt: {e}"))?;
    if disk_during_preedit != b"a" {
        return Err("SaveFile wrote preedit bytes to disk".into());
    }

    let preedit_search_hits = run_search_and_wait(
        window_handle,
        workspace,
        "an",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;
    if preedit_search_hits != 0 {
        return Err(format!(
            "workspace search matched uncommitted preedit 'an': hits={preedit_search_hits}"
        ));
    }

    // Verify mark remained active across search helper, and hide search panel without blurring editor.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.search.visible = false;
                ws.search.cancel_active();
                let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) else {
                    return Err("baseline_rev0 missing after search".into());
                };
                if !ed.has_active_ime(window, cx) || ed.editor().read(cx).text().to_string() != "an"
                {
                    return Err("active IME mark was lost across run_search_and_wait".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    // Cross the 220ms tick_background flush window while marked: recovery queue must NOT accept preedit.
    sleep_ms(260, cx).await;
    pump_frame(window_handle, cx)?;
    let stub_while_marked =
        query_manifest_stub(workspace, baseline_target.document.doc_id, cx).await?;
    if stub_while_marked.has_checkpoint || stub_while_marked.durable_rev != Revision(0) {
        return Err(format!(
            "RecoveryHandle accepted uncommitted preedit: has_checkpoint={} durable_rev={}",
            stub_while_marked.has_checkpoint, stub_while_marked.durable_rev.0
        ));
    }

    // Net no-op empty cancel at durable_revision 0 (no following edit).
    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            baseline_editor.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_no_events(&observer, "Phase 1A empty_cancel immediate", cx)?;

    let verify_empty_cancel = |window_handle: &AnyWindowHandle,
                               cx: &mut AsyncApp|
     -> Result<(), String> {
        window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| -> Result<(), String> {
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) else {
                        return Err("baseline_rev0 missing after empty_cancel".into());
                    };
                    let native_text = ed.editor().read(cx).text().to_string();
                    let same_instance =
                        ropes_are_instances(ed.editor().read(cx).text(), ed.committed_rope());
                    if native_text != "a"
                        || ed.committed_rope().to_string() != "a"
                        || !same_instance
                        || ed.has_pending_native_edit(cx)
                        || ed.has_active_ime(window, cx)
                        || ed.visible_revision != Revision(0)
                        || ed.queued_revision != Revision(0)
                        || ed.durable_revision != Revision(0)
                        || ed.has_checkpoint
                        || ed.is_dirty()
                        || ed.has_undo_history()
                    {
                        return Err(
                            "empty_cancel failed to restore clean Rope identity at rev 0".into(),
                        );
                    }
                    Ok(())
                })
            })
            .map_err(|e| e.to_string())?
    };
    verify_empty_cancel(window_handle, cx)?;
    pump_frame(window_handle, cx)?;
    sleep_ms(260, cx).await;
    pump_frame(window_handle, cx)?;
    assert_no_events(&observer, "Phase 1A empty_cancel post-settle", cx)?;
    verify_empty_cancel(window_handle, cx)?;

    // Undo after net no-op empty cancel must be a strict no-op (zero phantom undo history).
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    assert_no_events(&observer, "Phase 1A undo after empty_cancel", cx)?;
    let stub_after_cancel =
        query_manifest_stub(workspace, baseline_target.document.doc_id, cx).await?;
    if stub_after_cancel.has_checkpoint || stub_after_cancel.durable_rev != Revision(0) {
        return Err("empty_cancel created a phantom SQLite checkpoint".into());
    }

    // 1B. n -> ni -> 你 commit at durable_revision 0 (first_baseline immediate queue).
    mark_editor_text(window_handle, &baseline_editor, "n", cx)?;
    assert_no_events(&observer, "Phase 1B mark_n", cx)?;
    mark_editor_text(window_handle, &baseline_editor, "ni", cx)?;
    assert_no_events(&observer, "Phase 1B mark_ni", cx)?;

    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            baseline_editor.update(cx, |state, cx| {
                state.replace_text_in_range(None, "你", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    let commit_step =
        assert_single_final_event(&observer, "a你", &["an", "ani"], "Phase 1B commit 你", cx)?;
    if commit_step.start_byte != 1 || commit_step.old_end_byte != 1 || commit_step.new_end_byte != 4
    {
        return Err(format!(
            "commit 你 emitted unexpected byte span (expected 1..1 -> 1..4): {commit_step:?}"
        ));
    }

    // Because durable_revision was 0, finish_committed_mutation must synchronously queue rev 1.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) else {
                    return Err("baseline_rev0 missing after commit 你".into());
                };
                if ed.editor().read(cx).text().to_string() != "a你"
                    || ed.committed_rope().to_string() != "a你"
                    || ed.has_active_ime(window, cx)
                    || ed.has_pending_native_edit(cx)
                    || ed.visible_revision != Revision(1)
                    || ed.queued_revision != Revision(1)
                    || !ed.is_dirty()
                {
                    return Err(format!(
                        "first_baseline commit 你 state mismatch: vis={} queued={} dirty={} pending={}",
                        ed.visible_revision.0,
                        ed.queued_revision.0,
                        ed.is_dirty(),
                        ed.has_pending_native_edit(cx)
                    ));
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(baseline_target.view_id),
                    Some(TabSlot::Editable(ed))
                        if ed.durable_revision == Revision(1) && ed.has_checkpoint
                )
            })
        },
        "first_baseline durable ACK for commit 你",
    )
    .await?;

    let hydrated_rev1 = hydrate_durable_draft(workspace, baseline_target.document, cx).await?;
    if hydrated_rev1.durable_rev != Revision(1) || hydrated_rev1.rope.to_string() != "a你" {
        return Err(format!(
            "SQLite hydration mismatch at rev 1: rev={} text={:?}",
            hydrated_rev1.durable_rev.0,
            hydrated_rev1.rope.to_string()
        ));
    }

    let committed_hits = run_search_and_wait(
        window_handle,
        workspace,
        "你",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;
    let preedit_hits_after = run_search_and_wait(
        window_handle,
        workspace,
        "ani",
        SearchAlgorithm::MemchrBaseline,
        cx,
    )
    .await?;
    if committed_hits != 1 || preedit_hits_after != 0 {
        return Err(format!(
            "post-commit search mismatch: 你={committed_hits}, ani={preedit_hits_after}"
        ));
    }
    cx.update(|cx| {
        workspace.update(cx, |ws, _| {
            ws.search.visible = false;
            ws.search.cancel_active();
        });
    });

    // =========================================================================
    // Phase 2: Existing checkpoint (durable_revision > 0)
    // Atomic Undo/Redo via incremental TextDeltas + empty cancel on dirty draft.
    // =========================================================================
    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    let _ = cx.update(|cx| observer.update(cx, |o, _| o.take_snapshot()));

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;

    let undo_step =
        assert_single_final_event(&observer, "a", &["an", "ani"], "Phase 2 Undo of 你", cx)?;
    if undo_step.start_byte != 1 || undo_step.old_end_byte != 4 || undo_step.new_end_byte != 1 {
        return Err(format!(
            "Undo of 你 emitted unexpected byte span (expected 1..4 -> 1..1): {undo_step:?}"
        ));
    }

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(baseline_target.view_id),
                    Some(TabSlot::Editable(ed))
                        if ed.visible_revision == Revision(2)
                            && ed.durable_revision == Revision(2)
                            && !ed.is_dirty()
                            && !ed.has_pending_native_edit(cx)
                )
            })
        },
        "existing-checkpoint Undo delta durability and savepoint clear",
    )
    .await?;

    let hydrated_rev2 = hydrate_durable_draft(workspace, baseline_target.document, cx).await?;
    if hydrated_rev2.durable_rev != Revision(2) || hydrated_rev2.rope.to_string() != "a" {
        return Err("existing-checkpoint Undo delta hydration mismatch".into());
    }

    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) {
                    ed.redo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;

    let redo_step =
        assert_single_final_event(&observer, "a你", &["an", "ani"], "Phase 2 Redo of 你", cx)?;
    if redo_step.start_byte != 1 || redo_step.old_end_byte != 1 || redo_step.new_end_byte != 4 {
        return Err(format!(
            "Redo of 你 emitted unexpected byte span (expected 1..1 -> 1..4): {redo_step:?}"
        ));
    }

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(baseline_target.view_id),
                    Some(TabSlot::Editable(ed))
                        if ed.visible_revision == Revision(3)
                            && ed.durable_revision == Revision(3)
                            && ed.is_dirty()
                            && !ed.has_pending_native_edit(cx)
                )
            })
        },
        "existing-checkpoint Redo delta durability",
    )
    .await?;

    let hydrated_rev3 = hydrate_durable_draft(workspace, baseline_target.document, cx).await?;
    if hydrated_rev3.durable_rev != Revision(3) || hydrated_rev3.rope.to_string() != "a你" {
        return Err("existing-checkpoint Redo delta hydration mismatch".into());
    }

    // Empty-mark cancel on dirty existing-checkpoint draft preserves Revision(3) and dirty=true.
    mark_editor_text(window_handle, &baseline_editor, "z", cx)?;
    assert_no_events(&observer, "Phase 2B mark_z", cx)?;
    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            baseline_editor.update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    pump_frame(window_handle, cx)?;
    sleep_ms(260, cx).await;
    assert_no_events(&observer, "Phase 2B empty_cancel on dirty draft", cx)?;
    cx.update(|cx| -> Result<(), String> {
        let Some(TabSlot::Editable(ed)) = workspace.read(cx).view_slot(baseline_target.view_id)
        else {
            return Err("baseline_rev0 missing in Phase 2B".into());
        };
        if ed.visible_revision != Revision(3)
            || ed.queued_revision != Revision(3)
            || ed.durable_revision != Revision(3)
            || !ed.is_dirty()
            || ed.has_pending_native_edit(cx)
            || ed.committed_rope().to_string() != "a你"
        {
            return Err("empty_cancel on dirty existing checkpoint altered state".into());
        }
        Ok(())
    })?;

    // Second composition z -> zh -> 中 + single-event Undo/Redo.
    mark_editor_text(window_handle, &baseline_editor, "z", cx)?;
    mark_editor_text(window_handle, &baseline_editor, "zh", cx)?;
    assert_no_events(&observer, "Phase 2B mark_z_zh", cx)?;
    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            baseline_editor.update(cx, |state, cx| {
                state.replace_text_in_range(None, "中", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(
        &observer,
        "a你中",
        &["a你z", "a你zh"],
        "Phase 2B commit 中",
        cx,
    )?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(baseline_target.view_id),
                    Some(TabSlot::Editable(ed)) if ed.durable_revision == Revision(4)
                )
            })
        },
        "commit 中 durable rev 4",
    )
    .await?;

    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(&observer, "a你", &["a你z", "a你zh"], "Phase 2B Undo 中", cx)?;

    focus_editor_and_draw(window_handle, &baseline_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(baseline_target.view_id) {
                    ed.redo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(
        &observer,
        "a你中",
        &["a你z", "a你zh"],
        "Phase 2B Redo 中",
        cx,
    )?;

    report
        .committed_revisions_observed
        .extend([1, 2, 3, 4, 5, 6]);
    report.ime_commit_undo_redo_verified = true;
    drop(observer);

    // =========================================================================
    // Phase 3: unmark-only settlement + real SaveFile + BOM/CRLF format metadata
    // =========================================================================
    let unmark_save_path = work_dir.join("unmark_save.txt");
    std::fs::write(&unmark_save_path, b"\xEF\xBB\xBFa\r\n")
        .map_err(|e| format!("write unmark_save.txt: {e}"))?;
    let (unmark_target, unmark_editor) = open_owned_editable(
        window_handle,
        workspace,
        &unmark_save_path,
        "open unmark_save.txt",
        cx,
    )
    .await?;
    focus_editor_at_eof_and_draw(window_handle, &unmark_editor, cx)?;
    let unmark_observer =
        cx.update(|cx| cx.new(|cx| ImeEventObserver::for_editor(&unmark_editor, false, cx)));

    mark_editor_text(window_handle, &unmark_editor, "n", cx)?;
    assert_no_events(&unmark_observer, "Phase 3A mark_n", cx)?;

    focus_editor_and_draw(window_handle, &unmark_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            unmark_editor.update(cx, |state, cx| {
                state.unmark_text(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    assert_single_final_event(&unmark_observer, "a\r\nn", &[], "Phase 3A unmark_only", cx)?;
    let verify_unmark_settled = |window_handle: &AnyWindowHandle,
                                 cx: &mut AsyncApp|
     -> Result<(), String> {
        window_handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| -> Result<(), String> {
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(unmark_target.view_id) else {
                        return Err("unmark_save missing".into());
                    };
                    let native_text = ed.editor().read(cx).text().to_string();
                    let same_instance =
                        ropes_are_instances(ed.editor().read(cx).text(), ed.committed_rope());
                    if ed.has_active_ime(window, cx)
                        || ed.has_pending_native_edit(cx)
                        || !same_instance
                        || native_text != "a\r\nn"
                        || ed.committed_rope().to_string() != "a\r\nn"
                        || ed.visible_revision != Revision(1)
                        || ed.queued_revision != Revision(1)
                        || !ed.is_dirty()
                    {
                        return Err("unmark_text failed immediate settlement".into());
                    }
                    Ok(())
                })
            })
            .map_err(|e| e.to_string())?
    };
    verify_unmark_settled(window_handle, cx)?;
    // Paint-only turn with no following edit must keep identity settled.
    pump_frame(window_handle, cx)?;
    assert_no_events(&unmark_observer, "Phase 3A unmark paint_only", cx)?;
    verify_unmark_settled(window_handle, cx)?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(unmark_target.view_id),
                    Some(TabSlot::Editable(ed)) if ed.durable_revision == Revision(1)
                )
            })
        },
        "unmark_only durable rev 1",
    )
    .await?;

    let unmark_hydrated = hydrate_durable_draft(workspace, unmark_target.document, cx).await?;
    if unmark_hydrated.rope.to_string() != "a\r\nn" {
        return Err("unmark_only SQLite hydrated text mismatch".into());
    }

    // Undo and Redo of unmarked composition.
    focus_editor_and_draw(window_handle, &unmark_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(unmark_target.view_id) {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(&unmark_observer, "a\r\n", &[], "Phase 3A Undo unmark", cx)?;

    focus_editor_and_draw(window_handle, &unmark_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(unmark_target.view_id) {
                    ed.redo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(&unmark_observer, "a\r\nn", &[], "Phase 3A Redo unmark", cx)?;
    report.ime_unmark_and_empty_cancel_verified = true;
    drop(unmark_observer);

    // Real SaveFile action on unmark_save.txt (BOM + CRLF preserved; Save does not advance revision).
    focus_editor_and_draw(window_handle, &unmark_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(SaveFile), cx);
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.layout_dirty
                    && matches!(
                        ws.view_slot(unmark_target.view_id),
                        Some(TabSlot::Editable(ed)) if !ed.is_dirty()
                    )
            })
        },
        "SaveFile completion on unmark_save.txt",
    )
    .await?;

    let unmark_disk = std::fs::read(&unmark_save_path).map_err(|e| e.to_string())?;
    if unmark_disk != b"\xEF\xBB\xBFa\r\nn" {
        return Err(format!(
            "unmark_save.txt disk bytes mismatch: {unmark_disk:02X?}"
        ));
    }
    let unmark_saved_identity = cx.update(|cx| -> Result<ContentIdentity, String> {
        let Some(TabSlot::Editable(ed)) = workspace.read(cx).view_slot(unmark_target.view_id)
        else {
            return Err("unmark_save missing after save".into());
        };
        if ed.visible_revision != Revision(3)
            || ed.meta.saved_encoding != Some(EncodingSpec::Utf8Bom)
            || ed.meta.saved_line_ending != Some(LineEnding::Crlf)
            || compute_metadata_dirty(&ed.meta)
        {
            return Err("unmark_save post-save revision or format metadata mismatch".into());
        }
        ed.meta
            .saved_identity
            .ok_or_else(|| "missing unmark_save saved_identity".into())
    })?;

    // 3B. format_save.txt: UTF-8 + LF -> compose 你 -> on_toggle_bom + on_toggle_line_ending -> SaveFile.
    let format_save_path = work_dir.join("format_save.txt");
    std::fs::write(&format_save_path, b"base\n").map_err(|e| e.to_string())?;
    let (format_target, format_editor) = open_owned_editable(
        window_handle,
        workspace,
        &format_save_path,
        "open format_save.txt",
        cx,
    )
    .await?;
    focus_editor_at_eof_and_draw(window_handle, &format_editor, cx)?;
    mark_editor_text(window_handle, &format_editor, "n", cx)?;
    mark_editor_text(window_handle, &format_editor, "ni", cx)?;
    focus_editor_and_draw(window_handle, &format_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            format_editor.update(cx, |state, cx| {
                state.replace_text_in_range(None, "你", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    // Format toggles advance revision (+1 each -> Revision(3)); SaveFile itself advances +0.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.on_toggle_bom(&ToggleUtf8Bom, window, cx);
                ws.on_toggle_line_ending(&ToggleLineEnding, window, cx);
                let Some(TabSlot::Editable(ed)) = ws.view_slot(format_target.view_id) else {
                    return Err("format_save missing after format toggles".into());
                };
                if ed.visible_revision != Revision(3)
                    || ed.meta.encoding != EncodingSpec::Utf8Bom
                    || ed.meta.line_ending != LineEnding::Crlf
                    || !compute_metadata_dirty(&ed.meta)
                {
                    return Err(format!(
                        "format toggle revision/meta mismatch: rev={} enc={:?} eol={:?}",
                        ed.visible_revision.0, ed.meta.encoding, ed.meta.line_ending
                    ));
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    focus_editor_and_draw(window_handle, &format_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(SaveFile), cx);
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.layout_dirty
                    && matches!(
                        ws.view_slot(format_target.view_id),
                        Some(TabSlot::Editable(ed)) if !ed.is_dirty()
                    )
            })
        },
        "SaveFile completion on format_save.txt",
    )
    .await?;

    let format_disk = std::fs::read(&format_save_path).map_err(|e| e.to_string())?;
    if format_disk != b"\xEF\xBB\xBFbase\r\n\xE4\xBD\xA0" {
        return Err(format!(
            "format_save.txt disk bytes mismatch: {format_disk:02X?}"
        ));
    }
    let format_saved_identity = cx.update(|cx| -> Result<ContentIdentity, String> {
        let Some(TabSlot::Editable(ed)) = workspace.read(cx).view_slot(format_target.view_id)
        else {
            return Err("format_save missing after SaveFile".into());
        };
        if ed.visible_revision != Revision(3)
            || ed.meta.saved_encoding != Some(EncodingSpec::Utf8Bom)
            || ed.meta.saved_line_ending != Some(LineEnding::Crlf)
            || compute_metadata_dirty(&ed.meta)
        {
            return Err("SaveFile unexpectedly advanced revision or left metadata dirty".into());
        }
        ed.meta
            .saved_identity
            .ok_or_else(|| "missing format_save saved_identity".into())
    })?;

    let format_stub = query_manifest_stub(workspace, format_target.document.doc_id, cx).await?;
    if format_stub.meta.saved_encoding != Some(EncodingSpec::Utf8Bom)
        || format_stub.meta.saved_line_ending != Some(LineEnding::Crlf)
    {
        return Err("SessionManifest stub did not persist saved BOM/CRLF metadata".into());
    }

    // Discard-close baseline_rev0.txt now that Phases 1-2 finished so remaining open tabs are exact.
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.request_close_view(baseline_target, window, cx);
                if let Some(seq) = ws.pending_lifecycle_sequence() {
                    ws.choose_discard(seq, window, cx);
                    window.close_dialog(cx);
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
                !ws.is_lifecycle_busy()
                    && !ws.documents.contains_key(&baseline_target.document.doc_id)
            })
        },
        "discard close baseline_rev0.txt",
    )
    .await?;

    // =========================================================================
    // Phase 4: Close dialog readonly freeze & actual Cancel, Keep/reopen, Discard
    // =========================================================================
    let lifecycle_path = work_dir.join("lifecycle_ime.txt");
    std::fs::write(&lifecycle_path, b"seed\n").map_err(|e| e.to_string())?;
    let (lifecycle_target, lifecycle_editor) = open_owned_editable(
        window_handle,
        workspace,
        &lifecycle_path,
        "open lifecycle_ime.txt",
        cx,
    )
    .await?;
    focus_editor_at_eof_and_draw(window_handle, &lifecycle_editor, cx)?;
    mark_editor_text(window_handle, &lifecycle_editor, "n", cx)?;
    mark_editor_text(window_handle, &lifecycle_editor, "ni", cx)?;
    focus_editor_and_draw(window_handle, &lifecycle_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            lifecycle_editor.update(cx, |state, cx| {
                state.replace_text_in_range(None, "你", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(lifecycle_target.view_id),
                    Some(TabSlot::Editable(ed)) if ed.durable_revision == Revision(1) && ed.is_dirty()
                )
            })
        },
        "lifecycle_ime durable rev 1",
    )
    .await?;

    let pre_close_snapshot =
        cx.update(|cx| super::groups::snapshot(workspace.read(cx), lifecycle_target))?;

    // 4A. Request close -> verify rendered readonly freeze across session -> actual dialog Cancel.
    focus_editor_and_draw(window_handle, &lifecycle_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(CloseActiveTab), cx);
        })
        .map_err(|e| e.to_string())?;

    pump_frame(window_handle, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                if !ws.is_lifecycle_busy() || ws.pending_lifecycle_sequence().is_none() {
                    return Err(
                        "CloseActiveTab on dirty IME draft did not enter confirmation".into(),
                    );
                }
                for slot in ws.documents.values() {
                    if let TabSlot::Editable(ed) = slot
                        && !ed.editor().read(cx).presentation().is_readonly()
                    {
                        return Err(
                            "whole-session readonly freeze did not survive frame draw".into()
                        );
                    }
                }
                let prog = ws.apply_view_atomic_edits(
                    lifecycle_target,
                    Revision(1),
                    &[(0..0, "blocked".to_string())],
                    window,
                    cx,
                );
                if prog != Err(ApplyEditsError::ReadOnly) {
                    return Err(format!(
                        "expected ReadOnly during close freeze, got {prog:?}"
                    ));
                }
                lifecycle_editor.update(cx, |state, cx| {
                    state.replace_and_mark_text_in_range(None, "x", Some(1..1), window, cx);
                    state.replace_text_in_range(None, "y", window, cx);
                });
                let Some(TabSlot::Editable(ed)) = ws.view_slot(lifecycle_target.view_id) else {
                    return Err("lifecycle_ime missing while frozen".into());
                };
                if ed.has_active_ime(window, cx)
                    || ed.has_pending_native_edit(cx)
                    || super::groups::snapshot(ws, lifecycle_target)? != pre_close_snapshot
                {
                    return Err("rendered readonly editor accepted mark/text mutation".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    super::groups::dispatch_dialog(window_handle, cx, false)?;
    wait_until(
        Duration::from_secs(5),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).is_lifecycle_busy()),
        "close dialog Cancel completion",
    )
    .await?;
    pump_frame(window_handle, cx)?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        for slot in ws.documents.values() {
            if let TabSlot::Editable(ed) = slot
                && ed.editor().read(cx).presentation().is_readonly()
            {
                return Err("dialog Cancel failed to restore editable state".into());
            }
        }
        if super::groups::snapshot(ws, lifecycle_target)? != pre_close_snapshot {
            return Err("dialog Cancel mutated protected IME draft".into());
        }
        Ok(())
    })?;

    // Verify Undo/Redo still works after Cancel restoration.
    focus_editor_and_draw(window_handle, &lifecycle_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(lifecycle_target.view_id) {
                    ed.undo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    focus_editor_and_draw(window_handle, &lifecycle_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(lifecycle_target.view_id) {
                    ed.redo(window, cx);
                }
            });
        })
        .map_err(|e| e.to_string())?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(lifecycle_target.view_id),
                    Some(TabSlot::Editable(ed))
                        if ed.visible_revision == Revision(3)
                            && ed.durable_revision == Revision(3)
                            && ed.committed_rope().to_string() == "seed\n你"
                )
            })
        },
        "post-Cancel Undo/Redo durable rev 3",
    )
    .await?;

    // 4B. Close with Keep Stored Draft -> reopen_recently_closed restores "seed\n你", disk untouched.
    let closed_before = cx.update(|cx| workspace.read(cx).recently_closed_count());
    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.request_close_view(lifecycle_target, window, cx)
            });
        })
        .map_err(|e| e.to_string())?;
    super::groups::dispatch_dialog(window_handle, cx, true)?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy()
                    && ws.recently_closed_count() == closed_before + 1
                    && !ws
                        .session_layout()
                        .views
                        .contains_key(&lifecycle_target.view_id)
            })
        },
        "Keep Stored Draft close acknowledgement",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.reopen_recently_closed(window, cx));
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy()
                    && !ws.layout_dirty
                    && matches!(
                        ws.active_slot(),
                        Some(TabSlot::Editable(ed))
                            if ed.doc_id == lifecycle_target.document.doc_id
                                && !ed.has_pending_native_edit(cx)
                    )
            })
        },
        "reopen_recently_closed hydration of lifecycle_ime.txt",
    )
    .await?;

    let reopened_lifecycle_target = cx.update(|cx| -> Result<ViewTarget, String> {
        let ws = workspace.read(cx);
        let view_id = ws.active_view_id().ok_or("missing reopened view_id")?;
        let target = ws.view_target(view_id).ok_or("missing reopened target")?;
        let Some(TabSlot::Editable(ed)) = ws.active_slot() else {
            return Err("reopened lifecycle_ime not Editable".into());
        };
        if target.view_id == lifecycle_target.view_id
            || ed.committed_rope().to_string() != "seed\n你"
            || !ed.is_dirty()
            || ed.durable_revision != Revision(3)
        {
            return Err("reopened lifecycle_ime draft state mismatch".into());
        }
        Ok(target)
    })?;
    if std::fs::read(&lifecycle_path).map_err(|e| e.to_string())? != b"seed\n" {
        return Err("unsaved lifecycle_ime.txt disk file was unexpectedly modified".into());
    }

    // 4C. Transient tab discard_ime.txt -> compose 你 -> close with Discard (tombstoned).
    let discard_path = work_dir.join("discard_ime.txt");
    std::fs::write(&discard_path, b"throwaway\n").map_err(|e| e.to_string())?;
    let (discard_target, discard_editor) = open_owned_editable(
        window_handle,
        workspace,
        &discard_path,
        "open discard_ime.txt",
        cx,
    )
    .await?;
    let tombstoned_doc_id = discard_target.document.doc_id;
    focus_editor_at_eof_and_draw(window_handle, &discard_editor, cx)?;
    mark_editor_text(window_handle, &discard_editor, "n", cx)?;
    mark_editor_text(window_handle, &discard_editor, "ni", cx)?;
    focus_editor_and_draw(window_handle, &discard_editor, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            discard_editor.update(cx, |state, cx| {
                state.replace_text_in_range(None, "你", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(discard_target.view_id),
                    Some(TabSlot::Editable(ed)) if ed.durable_revision == Revision(1) && ed.is_dirty()
                )
            })
        },
        "discard_ime durable rev 1",
    )
    .await?;

    window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.request_close_view(discard_target, window, cx);
                let seq = ws
                    .pending_lifecycle_sequence()
                    .ok_or("missing discard lifecycle sequence")?;
                ws.choose_discard(seq, window, cx);
                window.close_dialog(cx);
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
                !ws.is_lifecycle_busy() && !ws.documents.contains_key(&tombstoned_doc_id)
            })
        },
        "discard_ime tombstoned close",
    )
    .await?;

    report.ime_save_and_lifecycle_verified = true;

    // =========================================================================
    // Phase 5: Single-line InputState (search_query_input) native IME parity (C3.1)
    // =========================================================================
    let search_input = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.activate_view(reopened_lifecycle_target, window, cx);
                ws.open_search_bar(false, window, cx);
                ws.search.query_kind = SearchQueryKind::LiteralStreaming;
                let input = ws.search_query_input.clone();
                input.update(cx, |s, cx| {
                    s.set_value("", window, cx);
                    s.focus(window, cx);
                });
                ws.run_search_from_inputs(window, cx);
                input
            })
        })
        .map_err(|e| e.to_string())?;
    pump_frame(window_handle, cx)?;

    let qid_base = cx.update(|cx| workspace.read(cx).search.next_query_id);
    let input_observer =
        cx.update(|cx| cx.new(|cx| ImeEventObserver::for_input(&search_input, cx)));

    for preedit in ["n", "ni"] {
        window_handle
            .update(cx, |_, window, cx| {
                search_input.update(cx, |s, cx| {
                    let end = preedit.encode_utf16().count();
                    s.replace_and_mark_text_in_range(None, preedit, Some(end..end), window, cx);
                });
            })
            .map_err(|e| e.to_string())?;
        assert_no_events(&input_observer, "Phase 5 Input preedit", cx)?;
        if cx.update(|cx| workspace.read(cx).search.next_query_id) != qid_base {
            return Err("InputState preedit unexpectedly refreshed workspace search".into());
        }
    }

    window_handle
        .update(cx, |_, window, cx| {
            search_input.update(cx, |s, cx| {
                s.replace_text_in_range(None, "你", window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(
        &input_observer,
        "你",
        &["n", "ni"],
        "Phase 5 Input commit 你",
        cx,
    )?;
    if cx.update(|cx| workspace.read(cx).search.next_query_id) != qid_base + 1 {
        return Err("InputState commit did not trigger exactly one search refresh".into());
    }
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).search.status,
                    SearchStatus::Done {
                        total_emitted: 1,
                        ..
                    }
                )
            })
        },
        "InputState search completion for 你",
    )
    .await?;

    // Single-step Undo ("你" -> "") and Redo ("" -> "你") on focused InputState.
    pump_frame(window_handle, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(Undo), cx);
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(&input_observer, "", &["n", "ni"], "Phase 5 Input Undo", cx)?;
    if cx.update(|cx| workspace.read(cx).search.status.clone()) != SearchStatus::Idle {
        return Err("InputState Undo to empty did not clear search results to Idle".into());
    }

    pump_frame(window_handle, cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(Redo), cx);
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(
        &input_observer,
        "你",
        &["n", "ni"],
        "Phase 5 Input Redo",
        cx,
    )?;
    if cx.update(|cx| workspace.read(cx).search.next_query_id) != qid_base + 2 {
        return Err("InputState Redo did not trigger exactly one search refresh".into());
    }
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).search.is_running()),
        "InputState Redo search settle",
    )
    .await?;

    // unmark_text on InputState -> exactly one Change & search refresh.
    window_handle
        .update(cx, |_, window, cx| {
            search_input.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(None, "k", Some(1..1), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_no_events(&input_observer, "Phase 5 Input mark_k", cx)?;
    if cx.update(|cx| workspace.read(cx).search.next_query_id) != qid_base + 2 {
        return Err("InputState mark_k refreshed search".into());
    }

    window_handle
        .update(cx, |_, window, cx| {
            search_input.update(cx, |s, cx| {
                s.unmark_text(window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_single_final_event(&input_observer, "你k", &[], "Phase 5 Input unmark", cx)?;
    if cx.update(|cx| workspace.read(cx).search.next_query_id) != qid_base + 3 {
        return Err("InputState unmark_text did not trigger exactly one search refresh".into());
    }
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).search.is_running()),
        "InputState unmark search settle",
    )
    .await?;

    // Empty-mark cancel on InputState -> zero Change, zero search refresh, Rope identity restored.
    let input_rope_before_cancel: Rope = cx.update(|cx| search_input.read(cx).text().clone());
    window_handle
        .update(cx, |_, window, cx| {
            search_input.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(None, "q", Some(1..1), window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_no_events(&input_observer, "Phase 5 Input mark_q", cx)?;
    window_handle
        .update(cx, |_, window, cx| {
            search_input.update(cx, |s, cx| {
                s.replace_and_mark_text_in_range(None, "", None, window, cx);
            });
        })
        .map_err(|e| e.to_string())?;
    assert_no_events(&input_observer, "Phase 5 Input empty_cancel", cx)?;
    window_handle
        .update(cx, |_, window, cx| -> Result<(), String> {
            let marked = search_input.update(cx, |s, cx| s.marked_text_range(window, cx));
            let same_instance =
                ropes_are_instances(search_input.read(cx).text(), &input_rope_before_cancel);
            if marked.is_some()
                || !same_instance
                || workspace.read(cx).search.next_query_id != qid_base + 3
            {
                return Err(
                    "InputState empty_cancel failed identity or search-refresh guard".into(),
                );
            }
            workspace.update(cx, |ws, cx| ws.close_overlays(window, cx));
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    pump_frame(window_handle, cx)?;
    drop(input_observer);

    report.ime_input_parity_verified = true;

    // =========================================================================
    // Phase 6: Bounded 32-cycle × 6-callback native benchmarks (C3.2)
    // ~1 MiB Rust, 16 MiB plaintext, 64 MiB - 4 B (67,108,860 B) plaintext
    // =========================================================================
    let bench_specs: [(&'static str, &'static str, Option<usize>); 3] = [
        ("ordinary_1mib_rust", "ime_bench_1mib.rs", None),
        (
            "ordinary_16mib_plaintext",
            "ime_bench_16mib.txt",
            Some(16 * 1024 * 1024),
        ),
        (
            "ordinary_64mib_minus_4b_plaintext",
            "ime_bench_64mib.txt",
            Some(NEAR_64MIB_ORDINARY_BYTES),
        ),
    ];

    for (dataset_name, file_name, plain_len) in bench_specs {
        let bench_get_bench = run_single_ime_dataset_benchmark(
            window_handle,
            workspace,
            &work_dir,
            dataset_name,
            file_name,
            plain_len,
            cx,
        )
        .await?;
        report.ime_benchmarks.push(bench_get_bench);
    }

    // =========================================================================
    // Final Layout & Post-Shutdown Expectation Capture (AFTER all benchmarks, C3.3)
    // =========================================================================
    let (recovery, expected_layout, dirty_rev, dirty_identity) = window_handle
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                ws.activate_view(reopened_lifecycle_target, window, cx);
                let Some(TabSlot::Editable(ed)) = ws.view_slot(reopened_lifecycle_target.view_id)
                else {
                    return Err("reopened lifecycle_ime missing before final layout capture".into());
                };
                if !ed.is_dirty()
                    || ed.has_pending_native_edit(cx)
                    || ed.durable_revision != ed.visible_revision
                {
                    return Err("lifecycle_ime not settled dirty/durable before shutdown".into());
                }
                let dirty_rev = ed.visible_revision;
                let dirty_identity = ContentIdentity::of_rope(ed.committed_rope());
                ws.capture_session_view_states(cx);
                ws.persist_session_layout(cx);
                let recovery = ws.recovery.clone().ok_or("missing RecoveryHandle")?;
                Ok((
                    recovery,
                    ws.session_layout().clone(),
                    dirty_rev,
                    dirty_identity,
                ))
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window_handle, cx)?;
    super::groups::wait_for_published_layout(&recovery, &expected_layout, cx).await?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).layout_dirty),
        "final IME session layout acknowledgement",
    )
    .await?;

    report.expected_session_layout = Some(expected_layout.clone());
    report.expected_ime_post_shutdown = Some(ImePostShutdownExpectation {
        expected_layout,
        tombstoned_doc_id,
        dirty_unsaved_key: reopened_lifecycle_target.document,
        dirty_unsaved_rev: dirty_rev,
        dirty_unsaved_identity: dirty_identity,
        dirty_unsaved_disk_path: lifecycle_path,
        saved_format_doc_id: format_target.document.doc_id,
        saved_format_identity: format_saved_identity,
        saved_format_path: format_save_path,
        unmark_saved_doc_id: unmark_target.document.doc_id,
        unmark_saved_identity,
        unmark_saved_path: unmark_save_path,
        cleanup_dir: work_dir,
    });

    Ok(())
}

async fn run_single_ime_dataset_benchmark(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    work_dir: &Path,
    dataset: &'static str,
    file_name: &'static str,
    plaintext_exact_bytes: Option<usize>,
    cx: &mut AsyncApp,
) -> Result<ImeDatasetBenchmark, String> {
    let raw_path = work_dir.join(file_name);
    let raw_for_write = raw_path.clone();
    let written_bytes = cx
        .background_executor()
        .spawn(async move {
            match plaintext_exact_bytes {
                Some(exact) => {
                    write_deterministic_plaintext_fixture(&raw_for_write, exact)?;
                    Ok::<usize, std::io::Error>(exact)
                }
                None => write_deterministic_1mib_rust_fixture(&raw_for_write),
            }
        })
        .await
        .map_err(|e| format!("generate {dataset} fixture: {e}"))?;

    let bench_path = std::fs::canonicalize(&raw_path)
        .map_err(|e| format!("{dataset}: resolve owned benchmark fixture: {e}"))?;
    let (bench_target, editor_entity) =
        open_owned_editable(window, workspace, &bench_path, dataset, cx).await?;
    let bench_doc_id = bench_target.document.doc_id;

    let rev_before = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<Revision, String> {
                let Some(TabSlot::Editable(ed)) = ws.view_slot(bench_target.view_id) else {
                    return Err(format!("{dataset}: not admitted as ordinary Editable"));
                };
                if ed.is_readonly() || ed.committed_byte_len() != written_bytes as u64 {
                    return Err(format!(
                        "{dataset}: admission mismatch readonly={} bytes={} expected={written_bytes}",
                        ed.is_readonly(),
                        ed.committed_byte_len()
                    ));
                }
                let mid = ed
                    .committed_rope()
                    .floor_char_boundary(ed.committed_rope().len() / 2);
                editor_entity.update(cx, |state, cx| {
                    state.set_selected_range(mid..mid, cx);
                    state.focus(window, cx);
                });
                Ok(ed.visible_revision)
            })
        })
        .map_err(|e| e.to_string())??;

    pump_frame(window, cx)?;

    // Counter-only observer inside timing: zero String/Vec allocation, zero full-buffer hashing.
    let bench_observer =
        cx.update(|cx| cx.new(|cx| ImeEventObserver::for_editor(&editor_entity, true, cx)));
    let mut op_samples: [Vec<f64>; IME_BENCH_OPS_PER_CYCLE] =
        std::array::from_fn(|_| Vec::with_capacity(IME_BENCH_CYCLES));

    for _ in 0..IME_BENCH_CYCLES {
        for op_idx in 0..IME_BENCH_OPS_PER_CYCLE {
            let t0 = Instant::now();
            window
                .update(cx, |_, window, cx| match op_idx {
                    0 => editor_entity.update(cx, |state, cx| {
                        state.replace_and_mark_text_in_range(None, "n", Some(1..1), window, cx);
                    }),
                    1 => editor_entity.update(cx, |state, cx| {
                        state.replace_and_mark_text_in_range(None, "ni", Some(2..2), window, cx);
                    }),
                    2 => editor_entity.update(cx, |state, cx| {
                        state.replace_text_in_range(None, "你", window, cx);
                    }),
                    3 => window.dispatch_action(Box::new(Undo), cx),
                    4 => editor_entity.update(cx, |state, cx| {
                        state.replace_and_mark_text_in_range(None, "x", Some(1..1), window, cx);
                    }),
                    _ => editor_entity.update(cx, |state, cx| {
                        state.replace_and_mark_text_in_range(None, "", None, window, cx);
                    }),
                })
                .map_err(|e| e.to_string())?;
            op_samples[op_idx].push(t0.elapsed().as_secs_f64() * 1000.0);
        }
    }

    let (edits_count, steps_count, changes_count) = cx.update(|cx| {
        let o = bench_observer.read(cx);
        (
            o.edits_event_count,
            o.edits_step_count,
            o.change_event_count,
        )
    });
    drop(bench_observer);

    if edits_count != IME_BENCH_EXPECTED_REV_DELTA as usize
        || steps_count != IME_BENCH_EXPECTED_REV_DELTA as usize
        || changes_count != IME_BENCH_EXPECTED_REV_DELTA as usize
    {
        return Err(format!(
            "{dataset}: expected 64 single-step committed events, got edits={edits_count} steps={steps_count} changes={changes_count}"
        ));
    }

    let expected_final_rev = Revision(rev_before.0 + IME_BENCH_EXPECTED_REV_DELTA);
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if let Some(err) = &ws.recovery_degraded_error {
            return Err(format!("{dataset}: unexpected recovery_degraded_error: {err}"));
        }
        let Some(TabSlot::Editable(ed)) = ws.view_slot(bench_target.view_id) else {
            return Err(format!("{dataset}: slot missing after benchmark loop"));
        };
        if ed.visible_revision != expected_final_rev
            || ed.committed_byte_len() != written_bytes as u64
            || ed.has_pending_native_edit(cx)
            || ed.oversized_recovery_warned
        {
            return Err(format!(
                "{dataset}: post-loop state mismatch rev={} expected={} bytes={} pending={} oversized_warn={}",
                ed.visible_revision.0,
                expected_final_rev.0,
                ed.committed_byte_len(),
                ed.has_pending_native_edit(cx),
                ed.oversized_recovery_warned
            ));
        }
        Ok(())
    })?;

    // Wait for actual durable recovery drain AND off-UI-thread savepoint identity verification (!is_dirty).
    wait_until(
        Duration::from_secs(30),
        cx,
        |cx| {
            cx.update(|cx| {
                matches!(
                    workspace.read(cx).view_slot(bench_target.view_id),
                    Some(TabSlot::Editable(ed))
                        if ed.durable_revision == expected_final_rev
                            && !ed.is_dirty()
                            && !ed.has_pending_native_edit(cx)
                )
            })
        },
        "benchmark durable drain and savepoint match",
    )
    .await?;

    // Close clean benchmark tab normally (no choose_discard outside Confirming, C3.2).
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.request_close_view(bench_target, window, cx));
        })
        .map_err(|e| e.to_string())?;

    wait_until(
        Duration::from_secs(20),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy()
                    && !ws.layout_dirty
                    && !ws.documents.contains_key(&bench_doc_id)
            })
        },
        "benchmark tab clean close before unlink",
    )
    .await?;

    std::fs::remove_file(&bench_path)
        .map_err(|e| format!("{dataset}: remove owned benchmark fixture: {e}"))?;

    let ops = OP_NAMES
        .iter()
        .zip(op_samples.iter_mut())
        .map(|(&operation, samples)| {
            samples.sort_by(f64::total_cmp);
            ImeOpQuantiles {
                operation,
                sample_count: samples.len(),
                p50_ms: percentile_sorted(samples, 0.50),
                p95_ms: percentile_sorted(samples, 0.95),
                max_ms: samples.last().copied().unwrap_or(0.0),
            }
        })
        .collect();

    Ok(ImeDatasetBenchmark {
        dataset,
        latency_label: TYPING_LATENCY_LABEL,
        exact_doc_bytes: written_bytes as u64,
        ordinary_admitted: true,
        cycles: IME_BENCH_CYCLES,
        total_samples: IME_BENCH_TOTAL_SAMPLES,
        committed_revision_delta: IME_BENCH_EXPECTED_REV_DELTA,
        final_bytes_unchanged: true,
        durable_drained: true,
        ops,
    })
}

pub(crate) fn verify_post_shutdown_ime(
    data_dir: &Path,
    report: &SmokeReport,
) -> Result<(), String> {
    let exp = report
        .expected_ime_post_shutdown
        .as_ref()
        .ok_or("missing expected_ime_post_shutdown payload")?;

    let db_path = data_dir.join("recovery.sqlite3");
    let handle = RecoveryHandle::start_with_faults(&db_path, FaultInjector::default())
        .map_err(|e| format!("post-shutdown IME RecoveryHandle::start failed: {e}"))?;

    let verify_res = (|| -> Result<(), String> {
        let manifest = handle
            .request_manifest()
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;

        if manifest.layout != exp.expected_layout {
            return Err("post-shutdown IME SessionLayout differs from captured layout".into());
        }
        if manifest.documents.len() != exp.expected_layout.views.len() {
            return Err("post-shutdown IME document/view count mismatch".into());
        }

        if manifest
            .documents
            .iter()
            .any(|s| s.doc_id == exp.tombstoned_doc_id)
        {
            return Err("discarded IME document still present in SessionManifest".into());
        }
        let closed_page = handle
            .request_recently_closed(None, 64)
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if closed_page
            .entries
            .iter()
            .any(|e| e.document.doc_id == exp.tombstoned_doc_id)
        {
            return Err("discarded IME document leaked into closed_views".into());
        }

        // Verify dirty unsaved IME draft ("seed\n你") survived shutdown while disk stayed "seed\n".
        let dirty_stub = manifest
            .documents
            .iter()
            .find(|s| s.key() == exp.dirty_unsaved_key)
            .ok_or("dirty unsaved IME draft missing from post-shutdown manifest")?;
        if !dirty_stub.has_checkpoint || dirty_stub.durable_rev < exp.dirty_unsaved_rev {
            return Err("dirty unsaved IME draft missing durable checkpoint after shutdown".into());
        }
        let dirty_draft = handle
            .request_hydrate(exp.dirty_unsaved_key, CancelToken::new())
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if dirty_draft.identity != exp.dirty_unsaved_identity
            || dirty_draft.rope.to_string() != "seed\n你"
        {
            return Err(format!(
                "post-shutdown hydrated IME draft mismatch: {:?}",
                dirty_draft.rope.to_string()
            ));
        }
        let unsaved_disk = std::fs::read(&exp.dirty_unsaved_disk_path)
            .map_err(|e| format!("read unsaved disk file: {e}"))?;
        if unsaved_disk != b"seed\n" {
            return Err("unsaved IME draft modified its backing disk file across shutdown".into());
        }

        // Verify saved BOM+CRLF format metadata and on-disk bytes for format_save.txt & unmark_save.txt.
        let format_stub = manifest
            .documents
            .iter()
            .find(|s| s.doc_id == exp.saved_format_doc_id)
            .ok_or("format_save.txt missing from post-shutdown manifest")?;
        if format_stub.meta.encoding != EncodingSpec::Utf8Bom
            || format_stub.meta.line_ending != LineEnding::Crlf
            || format_stub.meta.saved_encoding != Some(EncodingSpec::Utf8Bom)
            || format_stub.meta.saved_line_ending != Some(LineEnding::Crlf)
            || format_stub.meta.saved_identity != Some(exp.saved_format_identity)
            || compute_metadata_dirty(&format_stub.meta)
        {
            return Err("post-shutdown format_save.txt stub lost saved BOM/CRLF baseline".into());
        }
        if std::fs::read(&exp.saved_format_path).map_err(|e| e.to_string())?
            != b"\xEF\xBB\xBFbase\r\n\xE4\xBD\xA0"
        {
            return Err("post-shutdown format_save.txt disk bytes mismatch".into());
        }

        let unmark_stub = manifest
            .documents
            .iter()
            .find(|s| s.doc_id == exp.unmark_saved_doc_id)
            .ok_or("unmark_save.txt missing from post-shutdown manifest")?;
        if unmark_stub.meta.encoding != EncodingSpec::Utf8Bom
            || unmark_stub.meta.line_ending != LineEnding::Crlf
            || unmark_stub.meta.saved_encoding != Some(EncodingSpec::Utf8Bom)
            || unmark_stub.meta.saved_line_ending != Some(LineEnding::Crlf)
            || unmark_stub.meta.saved_identity != Some(exp.unmark_saved_identity)
            || compute_metadata_dirty(&unmark_stub.meta)
        {
            return Err("post-shutdown unmark_save.txt stub lost saved BOM/CRLF baseline".into());
        }
        if std::fs::read(&exp.unmark_saved_path).map_err(|e| e.to_string())?
            != b"\xEF\xBB\xBFa\r\nn"
        {
            return Err("post-shutdown unmark_save.txt disk bytes mismatch".into());
        }

        Ok(())
    })();

    let shutdown_res = handle.shutdown().map_err(|e| e.to_string()).and_then(|rx| {
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    });

    verify_res.and(shutdown_res)?;
    std::fs::remove_dir_all(&exp.cleanup_dir)
        .map_err(|e| format!("remove owned smoke_ime_artifacts directory: {e}"))
}
