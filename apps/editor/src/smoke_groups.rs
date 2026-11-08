use super::*;
use editor_services::{
    ContentIdentity, ErrorKind, RecoveryCatalogSummary, RecoveryEntry, RecoveryEntryCategory,
    RecoveryEntryKey, RecoveryEntryOrigin, RecoveryPageRequest, ViewState,
};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct EditorSnapshot {
    target: ViewTarget,
    entity: gpui_kit::EntityId,
    visible: Revision,
    queued: Revision,
    durable: Revision,
    text_sequence: u64,
    dirty: bool,
    undo_history: bool,
    text: String,
}
pub(super) fn snapshot(ws: &EditorWorkspace, target: ViewTarget) -> Result<EditorSnapshot, String> {
    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
        return Err("group fixture is not Editable".into());
    };
    if ed.doc_id != target.document.doc_id || ed.lifecycle_gen != target.document.lifecycle_gen {
        return Err("group fixture identity changed".into());
    }
    Ok(EditorSnapshot {
        target,
        entity: ed.editor().entity_id(),
        visible: ed.visible_revision,
        queued: ed.queued_revision,
        durable: ed.durable_revision,
        text_sequence: ed.current_text_seq(),
        dirty: ed.is_dirty(),
        undo_history: ed.has_undo_history(),
        text: ed.committed_rope().to_string(),
    })
}
pub(super) fn dispatch_dialog(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    accept: bool,
) -> Result<(), String> {
    pump_frame(window, cx)?;
    window
        .update(cx, |_, window, cx| {
            if accept {
                window.dispatch_action(
                    Box::new(gpui_kit::component::dialog::Confirm { secondary: false }),
                    cx,
                );
            } else {
                window.dispatch_action(Box::new(gpui_kit::component::dialog::Cancel), cx);
            }
        })
        .map_err(|e| e.to_string())
}

async fn wait_for_published_view(
    workspace: &Entity<EditorWorkspace>,
    target: ViewTarget,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let recovery = cx
        .update(|cx| workspace.read(cx).recovery.clone())
        .ok_or("missing recovery worker")?;
    cx.background_executor()
        .spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                match recovery.request_manifest() {
                    Ok(reply) => {
                        let manifest = reply
                            .recv_timeout(Duration::from_secs(5))
                            .map_err(|error| error.to_string())?
                            .map_err(|error| error.to_string())?;
                        if manifest.layout.target(target.view_id) == Some(target) {
                            return Ok(());
                        }
                    }
                    Err(error) if error.kind == ErrorKind::Busy => {}
                    Err(error) => return Err(error.to_string()),
                }
                if Instant::now() >= deadline {
                    return Err(
                        "native fixture view never reached acknowledged session metadata".into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .await
}

async fn run_batch_open_regression(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<bool, String> {
    let original = cx.update(|cx| {
        let ws = workspace.read(cx);
        if ws.session_layout().view_count() != 1 || ws.session_layout().panes().count() != 1 {
            return None;
        }
        let target = ws.active_view_id().and_then(|id| ws.view_target(id))?;
        match ws.view_slot(target.view_id) {
            Some(TabSlot::Editable(ed))
                if ed.meta.path.is_none()
                    && ed.visible_revision == Revision(0)
                    && ed.current_text_seq() == 0
                    && ed.committed_byte_len() == 0
                    && !ed.is_dirty()
                    && !ed.has_checkpoint
                    && !ed.has_undo_history()
                    && !ed.has_pending_native_edit(cx)
                    && ed.meta.encoding == EncodingSpec::Utf8
                    && ed.meta.line_ending == LineEnding::Lf
                    && ed.meta.language_override.is_none()
                    && ed.meta.saved_identity.is_none()
                    && ed.meta.origin_fingerprint.is_none()
                    && ed.meta.saved_encoding.is_none()
                    && ed.meta.saved_line_ending.is_none()
                    && ed.meta.readonly_reason.is_none()
                    && !ed.meta.paged =>
            {
                Some(target)
            }
            _ => None,
        }
    });
    // Populated restored fixtures still exercise the group scenario. Replacement
    // needs a genuine untouched scratch so this regression never discards data.
    let Some(original) = original else {
        return Ok(false);
    };
    wait_for_published_view(workspace, original, cx).await?;
    let directory = cx.update(|cx| {
        workspace
            .read(cx)
            .cli
            .data_dir
            .join("group_smoke_artifacts")
            .join(format!("{:032x}", DocumentId::new().0))
    });
    let (missing, paged, first, second) = cx
        .background_executor()
        .spawn(async move {
            std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
            let directory = std::fs::canonicalize(directory).map_err(|error| error.to_string())?;
            let paged = directory.join("long-line.txt");
            let first = directory.join("first.txt");
            let second = directory.join("second.txt");
            std::fs::write(&paged, vec![b'x'; 2 * 1024 * 1024])
                .map_err(|error| error.to_string())?;
            std::fs::write(&first, "batch first 中文\n").map_err(|error| error.to_string())?;
            std::fs::write(&second, "batch second 🙂\n").map_err(|error| error.to_string())?;
            Ok::<_, String>((directory.join("missing.txt"), paged, first, second))
        })
        .await?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.open_paths(vec![missing.clone(), paged.clone()], window, cx);
                if !ws.is_lifecycle_busy() {
                    return Err(
                        "published scratch Open did not enter staged lifecycle replacement".into(),
                    );
                }
                Ok(())
            })
        })
        .map_err(|error| error.to_string())??;
    wait_until(Duration::from_secs(30),cx,|cx|cx.update(|cx|{
        let ws=workspace.read(cx);
        !ws.is_lifecycle_busy()&&ws.session_layout().view_count()==1&&ws.hydrated_editor_count()==0
            &&matches!(ws.view_slot(original.view_id),Some(TabSlot::Paged(view)) if view.meta.path.as_ref()==Some(&paged)&&view.current_slice.is_some()&&matches!(view.index_status,editor_services::IndexStatus::Complete{..}))
    }),"failed first path drains into real Paged replacement, initial viewport and sparse index").await?;
    let paged_target = cx.update(|cx| -> Result<ViewTarget, String> {
        let ws = workspace.read(cx);
        let target = ws
            .view_target(original.view_id)
            .ok_or("replacement changed the stable ViewId")?;
        if target.document == original.document || !ws.recent_files().contains(&paged) {
            return Err("Paged replacement did not change document identity and record MRU".into());
        }
        Ok(target)
    })?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.request_close_view(paged_target, window, cx))
        })
        .map_err(|error| error.to_string())?;
    wait_until(Duration::from_secs(20),cx,|cx|cx.update(|cx|{
        let ws=workspace.read(cx);
        !ws.is_lifecycle_busy()&&ws.session_layout().view_count()==1
            &&matches!(ws.active_slot(),Some(TabSlot::Editable(ed)) if ed.meta.path.is_none()&&ed.visible_revision==Revision(0)&&!ed.has_checkpoint)
    }),"closing the clean Paged view creates a persisted scratch").await?;
    let (scratch, group) = cx.update(|cx| -> Result<_, String> {
        let ws = workspace.read(cx);
        let target = ws
            .active_view_id()
            .and_then(|id| ws.view_target(id))
            .ok_or("missing replacement scratch")?;
        Ok((target, ws.session_layout().views[&target.view_id].group_id))
    })?;
    wait_for_published_view(workspace, scratch, cx).await?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.open_paths(
                    vec![
                        missing.clone(),
                        first.clone(),
                        second.clone(),
                        first.clone(),
                    ],
                    window,
                    cx,
                )
            })
        })
        .map_err(|error| error.to_string())?;
    wait_until(Duration::from_secs(20),cx,|cx|cx.update(|cx|{
        let ws=workspace.read(cx);
        !ws.is_lifecycle_busy()&&ws.session_layout().view_count()==2
            &&[(&first,"batch first 中文\n"),(&second,"batch second 🙂\n")].iter().all(|(path,text)|ws.all_view_ids().any(|id|matches!(ws.view_slot(id),Some(TabSlot::Editable(ed)) if ed.meta.path.as_ref()==Some(*path)&&ed.committed_rope().to_string()==*text&&!ed.is_dirty()&&ed.visible_revision==Revision(0)&&!ed.has_undo_history())))
    }),"one failed staged path retains both accepted tail paths without duplicate or blank views").await?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let replacement = ws
            .view_target(scratch.view_id)
            .ok_or("first accepted file lost the scratch ViewId")?;
        if replacement.document == scratch.document
            || ws
                .session_layout()
                .views
                .values()
                .any(|view| view.group_id != group)
        {
            return Err(
                "accepted batch lost fresh document identity or its invocation group".into(),
            );
        }
        if !ws.recent_files().contains(&first) || !ws.recent_files().contains(&second) {
            return Err("accepted batch file missing from MRU".into());
        }
        Ok(())
    })?;
    let recovery = cx
        .update(|cx| workspace.read(cx).recovery.clone())
        .ok_or("missing recovery worker")?;
    cx.background_executor()
        .spawn(async move {
            for old in [original, scratch] {
                let reply = recovery
                    .request_hydrate(old.document, CancelToken::new())
                    .map_err(|error| error.to_string())?;
                let result = reply
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| error.to_string())?;
                match result {
                    Err(error) if error.kind == ErrorKind::Stale => {}
                    other => {
                        return Err(format!(
                            "published scratch was not permanently retired: {other:?}"
                        ));
                    }
                }
            }
            Ok::<_, String>(())
        })
        .await?;
    Ok(true)
}

pub(super) async fn wait_for_published_layout(
    recovery: &RecoveryHandle,
    expected: &SessionLayout,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let recovery = recovery.clone();
    let expected = expected.clone();
    cx.background_executor()
        .spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let manifest = recovery
                    .request_manifest()
                    .map_err(|e| e.to_string())?
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|e| e.to_string())?
                    .map_err(|e| e.to_string())?;
                if manifest.layout == expected {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("native bulk layout never reached exact durable publication".into());
                }
                // Match the existing publication waiter: delay only between failed
                // equality observations; elapsed time can never establish success.
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .await
}

#[derive(Debug)]
struct BulkStoredDraft {
    target: ViewTarget,
    catalog_key: RecoveryEntryKey,
    revision: Revision,
    meta: Vec<u8>,
    position: u16,
    view: ViewState,
}

#[derive(Clone, Debug)]
pub(crate) struct BulkGroupEvidence {
    group: GroupId,
    pane: editor_services::PaneId,
    drafts: Arc<HashMap<DocumentId, BulkStoredDraft>>,
    active: DocumentKey,
    active_identity: ContentIdentity,
    scratch: ViewTarget,
}

fn bulk_catalog(
    recovery: &RecoveryHandle,
    expected: RecoveryCatalogSummary,
) -> Result<HashMap<DocumentId, RecoveryEntry>, String> {
    let count = usize::try_from(expected.total_eligible).map_err(|e| e.to_string())?;
    if count > editor_services::MAX_SESSION_VIEWS + 1 {
        return Err("bulk evidence exceeds the bounded fixture catalog".into());
    }
    let mut records = HashMap::with_capacity(count);
    let mut cursor = None;
    for _ in 0..count.div_ceil(64).max(1) {
        let page = recovery
            .request_recovery_page(RecoveryPageRequest { cursor, limit: 64 })
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(20))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if page.summary != expected || page.page_unreadable != 0 {
            return Err(format!(
                "bulk catalog state differs: {:?}, unreadable={}",
                page.summary, page.page_unreadable
            ));
        }
        for entry in page.entries {
            let id = entry
                .key
                .doc_id
                .ok_or("bulk catalog has an invalid document identity")?;
            if entry.key.origin != RecoveryEntryOrigin::Document
                || records.insert(id, entry).is_some()
            {
                return Err("bulk catalog has an unexpected origin or duplicate document".into());
            }
        }
        match page.next {
            None if records.len() == count => return Ok(records),
            None => return Err("bulk catalog omitted expected documents".into()),
            Some(next) if Some(next) != cursor => cursor = Some(next),
            Some(_) => return Err("bulk catalog cursor did not advance".into()),
        }
    }
    Err("bulk catalog did not finish within its bounded page count".into())
}

fn bulk_summary(count: usize, closed: bool) -> RecoveryCatalogSummary {
    RecoveryCatalogSummary {
        total_eligible: count as u64 + u64::from(closed),
        admitted: if closed { 1 } else { count as u16 },
        closed: if closed { count as u64 } else { 0 },
        unplaced_checkpoints: 0,
        unplaced_other: 0,
    }
}

fn verify_bulk_catalog(
    recovery: &RecoveryHandle,
    group: GroupId,
    pane: editor_services::PaneId,
    expected: &HashMap<DocumentId, BulkStoredDraft>,
    scratch: Option<ViewTarget>,
) -> Result<(), String> {
    let records = bulk_catalog(recovery, bulk_summary(expected.len(), scratch.is_some()))?;
    for (id, draft) in expected {
        let record = records
            .get(id)
            .ok_or("bulk catalog lost a protected document")?;
        let stub = record.stub.as_ref().map_err(|e| e.to_string())?;
        let category = if scratch.is_some() {
            RecoveryEntryCategory::Closed
        } else {
            RecoveryEntryCategory::Admitted
        };
        if record.key != draft.catalog_key
            || record.category != category
            || stub.key() != draft.target.document
            || !record.has_checkpoint
            || !stub.has_checkpoint
            || stub.durable_rev != draft.revision
            || serde_json::to_vec(&stub.meta).map_err(|e| e.to_string())? != draft.meta
        {
            return Err("bulk operation changed a document identity, lifecycle, checkpoint revision, metadata, or durable state".into());
        }
        if scratch.is_some() {
            let placement = record
                .last_placement
                .as_ref()
                .ok_or("bulk Keep omitted closed placement")?;
            if placement.previous_view.id != draft.target.view_id
                || placement.pane_id != pane
                || placement.previous_view.document != draft.target.document
                || placement.previous_view.group_id != group
                || placement.previous_view.view != draft.view
                || placement.position != draft.position
                || placement.document.key() != draft.target.document
            {
                return Err(
                    "bulk Keep changed a preserved closed-view identity or position".into(),
                );
            }
        } else if record.last_placement.is_some() {
            return Err("bulk Cancel created a closed-view record".into());
        }
    }
    if let Some(scratch) = scratch {
        let record = records
            .get(&scratch.document.doc_id)
            .ok_or("bulk Keep scratch is absent from durable catalog")?;
        let stub = record.stub.as_ref().map_err(|e| e.to_string())?;
        if record.category != RecoveryEntryCategory::Admitted
            || stub.key() != scratch.document
            || record.has_checkpoint
            || stub.has_checkpoint
            || stub.durable_rev != Revision(0)
            || stub.meta.path.is_some()
            || compute_metadata_dirty(&stub.meta)
            || record.last_placement.is_some()
        {
            return Err("bulk Keep did not retain exactly one clean current scratch".into());
        }
    }
    Ok(())
}

fn verify_bulk_slots(
    workspace: &EditorWorkspace,
    layout: &SessionLayout,
    active: &EditorSnapshot,
    drafts: &HashMap<DocumentId, BulkStoredDraft>,
    readonly: bool,
    cx: &gpui_kit::App,
) -> Result<(), String> {
    if workspace.session_layout() != layout
        || workspace.documents.len() != drafts.len()
        || snapshot(workspace, active.target)? != *active
    {
        return Err(
            "bulk preflight/Cancel changed live view identities or the active draft".into(),
        );
    }
    for id in layout.view_ids() {
        let target = layout
            .target(id)
            .ok_or("bulk fixture has a dangling view")?;
        let expected = drafts
            .get(&target.document.doc_id)
            .ok_or("bulk fixture document missing from catalog")?;
        let slot = workspace
            .view_slot(id)
            .ok_or("bulk operation lost a live slot")?;
        match slot {
            TabSlot::Editable(editor) if target == active.target => {
                let actual = editor.editor().read(cx).presentation().is_readonly();
                if actual != readonly {
                    return Err(format!(
                        "bulk native readonly mismatch: expected={readonly}, actual={actual}, lifecycle={:?}",
                        workspace.pending_lifecycle_sequence(),
                    ));
                }
            }
            TabSlot::Dehydrated(stub)
                if target != active.target
                    && stub.has_checkpoint
                    && stub.durable_rev == expected.revision => {}
            _ => return Err("bulk operation hydrated or replaced an inactive draft".into()),
        }
        if serde_json::to_vec(slot.meta()).map_err(|e| e.to_string())? != expected.meta {
            return Err("bulk operation changed live document metadata".into());
        }
    }
    Ok(())
}

async fn verify_native_group_ui(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let evidence = crate::group_ui::verify_group_ui_native(window, workspace, cx).await?;
    report.group_drag_drop_verified = evidence.drag_drop_verified;
    report.group_drag_cancel_verified = evidence.drag_cancel_verified;
    report.group_keyboard_modal_verified = evidence.keyboard_modal_verified;
    if !evidence.drag_drop_verified
        || !evidence.drag_cancel_verified
        || !evidence.keyboard_modal_verified
    {
        return Err("native group UI verification returned incomplete evidence".into());
    }
    Ok(())
}

async fn run_bulk_group_scenario(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<bool, String> {
    let group = cx.update(|cx| {
        let layout = workspace.read(cx).session_layout();
        let mut groups = layout
            .tree
            .groups
            .iter()
            .filter(|group| group.id != GroupId::UNGROUPED);
        let group = groups.next()?.id;
        if groups.next().is_some()
            || layout
                .views
                .values()
                .filter(|view| view.group_id == group)
                .count()
                < 1000
        {
            return None;
        }
        Some(group)
    });
    let Some(group) = group else {
        return Ok(false);
    };
    if cx.update(|cx| workspace.read(cx).session_layout().panes().count() != 1) {
        return Err("bulk groups regression requires its single-pane fixture; use --smoke=splits for split fixtures".into());
    }
    verify_native_group_ui(window, workspace, report, cx).await?;
    pump_frame(window, cx)?;
    let (before, active, original_readonly, active_rope, recovery) = window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                ws.capture_session_view_states(cx);
                let layout = ws.session_layout();
                if !(1000..=editor_services::MAX_SESSION_VIEWS).contains(&layout.views.len())
                    || layout.views.values().any(|view| view.group_id != group)
                    || layout.active_group() != group
                {
                    return Err("bulk fixture must contain only the single protected group".into());
                }
                let target = ws
                    .active_view_id()
                    .and_then(|id| ws.view_target(id))
                    .ok_or("bulk fixture has no active view")?;
                let Some(TabSlot::Editable(editor)) = ws.view_slot(target.view_id) else {
                    return Err("bulk fixture active draft is not a real native editor".into());
                };
                let observed = (
                    layout.clone(),
                    snapshot(ws, target)?,
                    editor.editor().read(cx).presentation().is_readonly(),
                    editor.committed_rope().clone(),
                    ws.recovery
                        .clone()
                        .ok_or("bulk fixture recovery unavailable")?,
                );
                ws.persist_session_layout(cx);
                Ok(observed)
            })
        })
        .map_err(|e| e.to_string())??;
    wait_for_published_layout(&recovery, &before, cx).await?;
    let worker = recovery.clone();
    let layout = before.clone();
    let (drafts, active_identity) = cx
        .background_executor()
        .spawn(async move {
            let mut records = bulk_catalog(&worker, bulk_summary(layout.views.len(), false))?;
            let mut drafts = HashMap::with_capacity(layout.views.len());
            for (position, id) in layout.view_ids().enumerate() {
                let view = &layout.views[&id];
                let record = records
                    .remove(&view.document.doc_id)
                    .ok_or("bulk fixture catalog/view mismatch")?;
                let stub = record.stub.map_err(|e| e.to_string())?;
                if record.category != RecoveryEntryCategory::Admitted
                    || !record.has_checkpoint
                    || !stub.has_checkpoint
                    || stub.key() != view.document
                    || stub.meta.path.is_some()
                    || record.last_placement.is_some()
                {
                    return Err(
                        "bulk fixture is not a fresh set of durable protected untitled drafts"
                            .to_string(),
                    );
                }
                drafts.insert(
                    stub.doc_id,
                    BulkStoredDraft {
                        target: ViewTarget {
                            view_id: id,
                            document: view.document,
                        },
                        catalog_key: record.key,
                        revision: stub.durable_rev,
                        meta: serde_json::to_vec(&stub.meta).map_err(|e| e.to_string())?,
                        position: u16::try_from(position).map_err(|e| e.to_string())?,
                        view: view.view.clone(),
                    },
                );
            }
            Ok((Arc::new(drafts), ContentIdentity::of_rope(&active_rope)))
        })
        .await?;
    cx.update(|cx| {
        verify_bulk_slots(
            workspace.read(cx),
            &before,
            &active,
            &drafts,
            original_readonly,
            cx,
        )
    })?;

    for keep in [false, true] {
        pump_frame(window, cx)?;
        window
            .update(cx, |_, window, cx| {
                window.dispatch_action(Box::new(crate::actions::CloseCurrentGroupTabs), cx);
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(10),
            cx,
            |cx| cx.update(|cx| workspace.read(cx).pending_lifecycle_sequence().is_some()),
            "rendered native bulk Close Group action",
        )
        .await?;
        pump_frame(window, cx)?;
        cx.update(|cx| verify_bulk_slots(workspace.read(cx), &before, &active, &drafts, true, cx))?;
        // Exactly one native dialog decision must settle all members; never drive
        // per-document handlers or load inactive bodies to construct this proof.
        dispatch_dialog(window, cx, keep)?;
        let mut invariant_error = None;
        wait_until(
            Duration::from_secs(30),
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    for target in before.views.values().map(|view| ViewTarget {
                        view_id: view.id,
                        document: view.document,
                    }) {
                        if target != active.target
                            && ws
                                .view_slot(target.view_id)
                                .is_some_and(|slot| !matches!(slot, TabSlot::Dehydrated(_)))
                        {
                            invariant_error =
                                Some("bulk close hydrated an inactive draft".to_string());
                            return true;
                        }
                    }
                    let remaining = before
                        .views
                        .keys()
                        .filter(|id| ws.session_layout().views.contains_key(*id))
                        .count();
                    if (remaining != 0 && remaining != before.views.len())
                        || (ws.is_lifecycle_busy() && remaining != before.views.len())
                    {
                        invariant_error =
                            Some("bulk close exposed a partial/pre-ack layout".to_string());
                        return true;
                    }
                    !ws.is_lifecycle_busy()
                        && if keep {
                            remaining == 0 && ws.session_layout().views.len() == 1
                        } else {
                            remaining == before.views.len()
                        }
                })
            },
            if keep {
                "one aggregate Keep acknowledgment for every protected draft"
            } else {
                "one aggregate Cancel restoring every protected draft"
            },
        )
        .await?;
        if let Some(error) = invariant_error {
            return Err(error);
        }
        if !keep {
            pump_frame(window, cx)?;
            cx.update(|cx| {
                verify_bulk_slots(
                    workspace.read(cx),
                    &before,
                    &active,
                    &drafts,
                    original_readonly,
                    cx,
                )
            })?;
            wait_for_published_layout(&recovery, &before, cx).await?;
            let worker = recovery.clone();
            let expected = drafts.clone();
            let pane = before.active_pane().id;
            cx.background_executor()
                .spawn(async move { verify_bulk_catalog(&worker, group, pane, &expected, None) })
                .await?;
            report.group_bulk_cancel_verified = true;
        }
    }
    let scratch = window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                let scratch = ws
                    .active_view_id()
                    .and_then(|id| ws.view_target(id))
                    .ok_or("bulk Keep has no replacement scratch")?;
                let Some(TabSlot::Editable(editor)) = ws.view_slot(scratch.view_id) else {
                    return Err("bulk Keep scratch is not a real native editor".into());
                };
                if drafts.contains_key(&scratch.document.doc_id)
                    || scratch.document.lifecycle_gen != LifecycleGen(1)
                    || ws.documents.len() != 1
                    || editor.committed_byte_len() != 0
                    || editor.visible_revision != Revision(0)
                    || editor.has_checkpoint
                    || editor.is_dirty()
                    || editor.has_undo_history()
                    || editor.has_pending_native_edit(cx)
                    || editor.meta.path.is_some()
                    || ws.session_layout().active_group() != GroupId::UNGROUPED
                {
                    return Err(
                        "bulk Keep did not produce exactly one fresh clean Ungrouped scratch"
                            .into(),
                    );
                }
                ws.capture_session_view_states(cx);
                let mut after = before.clone();
                let targets = before
                    .view_ids()
                    .map(|id| before.target(id).unwrap())
                    .collect::<Vec<_>>();
                after.remove_views(&targets).map_err(|e| e.to_string())?;
                after
                    .insert_view(
                        after.active_pane_id(),
                        ws.session_layout().views[&scratch.view_id].clone(),
                        None,
                        true,
                    )
                    .map_err(|e| e.to_string())?;
                if ws.session_layout() != &after {
                    return Err(
                        "bulk Keep changed unaffected groups, pane identity, or membership".into(),
                    );
                }
                ws.persist_session_layout(cx);
                report.expected_session_layout = Some(after);
                Ok(scratch)
            })
        })
        .map_err(|e| e.to_string())??;
    wait_for_published_layout(
        &recovery,
        report
            .expected_session_layout
            .as_ref()
            .ok_or("missing bulk after-layout")?,
        cx,
    )
    .await?;
    let evidence = BulkGroupEvidence {
        group,
        pane: before.active_pane().id,
        drafts,
        active: active.target.document,
        active_identity,
        scratch,
    };
    let expected = evidence.clone();
    let worker = recovery.clone();
    cx.background_executor()
        .spawn(async move {
            verify_bulk_catalog(
                &worker,
                expected.group,
                expected.pane,
                &expected.drafts,
                Some(expected.scratch),
            )?;
            verify_bulk_active_content(&worker, &expected)
        })
        .await?;
    report.expected_bulk_group = Some(evidence);
    report.group_bulk_keep_verified = true;
    Ok(true)
}

fn verify_bulk_active_content(
    recovery: &RecoveryHandle,
    evidence: &BulkGroupEvidence,
) -> Result<(), String> {
    // Only the formerly active draft is read. The other >=999 bodies remain
    // unopened; their exact keyed state/head/placement metadata is checked above.
    let draft = recovery
        .request_hydrate(evidence.active, CancelToken::new())
        .map_err(|e| e.to_string())?
        .recv_timeout(Duration::from_secs(20))
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if draft.identity != evidence.active_identity {
        return Err("bulk Keep changed the formerly active draft's actual content".into());
    }
    Ok(())
}

pub(super) async fn run_group_scenario(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    run_startup_scenario(window, workspace, report, cx).await?;
    if run_bulk_group_scenario(window, workspace, report, cx).await? {
        return Ok(());
    }
    report.group_batch_open_verified = run_batch_open_regression(window, workspace, cx).await?;
    let samples = [
        "alpha 中文🙂\r\nsecond\r\n",
        "beta café\n",
        "gamma 日本語🙂\r\n",
    ];
    let mut targets = Vec::new();
    for sample in samples {
        let target = window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| -> Result<ViewTarget, String> {
                    let target = ws
                        .create_untitled_tab(true, window, cx)
                        .ok_or("group fixture view admission failed")?;
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                        return Err("group fixture hydration failed".into());
                    };
                    let editor = ed.editor().clone();
                    editor.update(cx, |state, cx| {
                        state.replace_text_in_range(None, sample, window, cx)
                    });
                    Ok(target)
                })
            })
            .map_err(|e| e.to_string())??;
        targets.push(target);
    }
    wait_until(Duration::from_secs(15),cx,|cx|cx.update(|cx|{let ws=workspace.read(cx);targets.iter().all(|target|matches!(ws.view_slot(target.view_id),Some(TabSlot::Editable(ed)) if ed.is_dirty()&&ed.visible_revision.0>0&&ed.durable_revision==ed.visible_revision))}),"three real native group drafts durable").await?;
    verify_native_group_ui(window, workspace, report, cx).await?;
    let before = cx.update(|cx| {
        targets
            .iter()
            .map(|target| snapshot(workspace.read(cx), *target))
            .collect::<Result<Vec<_>, _>>()
    })?;
    let group = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<GroupId, String> {
                let a = ws
                    .apply_group_command(
                        GroupCommand::Create {
                            name: "Alpha".into(),
                            color: GroupColor::Blue,
                            before: None,
                            initial_view: Some(targets[0]),
                        },
                        window,
                        cx,
                    )
                    .map_err(|e| e.to_string())?
                    .created_group
                    .ok_or("missing created group")?;
                let b = ws
                    .apply_group_command(
                        GroupCommand::Create {
                            name: "Beta".into(),
                            color: GroupColor::Pink,
                            before: None,
                            initial_view: Some(targets[1]),
                        },
                        window,
                        cx,
                    )
                    .map_err(|e| e.to_string())?
                    .created_group
                    .ok_or("missing second group")?;
                for command in [
                    GroupCommand::MoveView {
                        target: targets[2],
                        group: a,
                        before: Some(targets[0].view_id),
                    },
                    GroupCommand::Rename {
                        group: a,
                        name: "中文 Working Set".into(),
                    },
                    GroupCommand::SetColor {
                        group: a,
                        color: GroupColor::Teal,
                    },
                    GroupCommand::SetCollapsed {
                        group: a,
                        collapsed: true,
                    },
                    GroupCommand::Reorder {
                        group: a,
                        before: Some(GroupId::UNGROUPED),
                    },
                    GroupCommand::Remove { group: b },
                ] {
                    ws.apply_group_command(command, window, cx)
                        .map_err(|e| e.to_string())?;
                }
                ws.session_layout().validate().map_err(|e| e.to_string())?;
                if ws.session_layout().views[&targets[1].view_id].group_id != GroupId::UNGROUPED {
                    return Err("removing a group did not keep its view in Ungrouped".into());
                }
                if ws.active_view_id() != Some(targets[2].view_id)
                    || !ws.session_layout().group(a).unwrap().collapsed
                {
                    return Err("collapsing the active group changed active identity".into());
                }
                Ok(a)
            })
        })
        .map_err(|e| e.to_string())??;
    pump_frame(window, cx)?;
    sleep_ms(100, cx).await;
    let after = cx.update(|cx| {
        targets
            .iter()
            .map(|target| snapshot(workspace.read(cx), *target))
            .collect::<Result<Vec<_>, _>>()
    })?;
    if before != after {
        return Err(format!(
            "group-only changes altered editor entity/text/revision/dirty/undo state: before={before:?}, after={after:?}"
        ));
    }
    report.group_metadata_mutations_verified = true;

    // One aggregate confirmation freezes every editor, not just the affected group.
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.request_close_group(group, window, cx);
                if ws.pending_lifecycle_sequence().is_none() {
                    return Err("group close did not enter preflight".into());
                }
                let attempt = ws.apply_view_atomic_edits(
                    targets[0],
                    before[0].visible,
                    &[(0..0, "blocked".into())],
                    window,
                    cx,
                );
                if attempt != Err(ApplyEditsError::ReadOnly) {
                    return Err("programmatic edit bypassed lifecycle freeze".into());
                }
                ws.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
                if ws.view_slot(targets[2].view_id).unwrap().meta().encoding != EncodingSpec::Utf8 {
                    return Err("format mutation bypassed lifecycle freeze".into());
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    // A real render must preserve the core-owned readonly bit; checking only
    // immediately after set_readonly misses a facade that overwrites it.
    pump_frame(window, cx)?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                for (target, expected) in targets.iter().zip(&before) {
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                        return Err("rendered freeze replaced an editor".into());
                    };
                    if !ed.editor().read(cx).presentation().is_readonly() {
                        return Err("whole-session readonly freeze did not survive rendering".into());
                    }
                    let editor = ed.editor().clone();
                    editor.update(cx, |state, cx| {
                        state.replace_text_in_range(Some(0..0), "blocked native edit", window, cx);
                    });
                    if matches!(ws.view_slot(target.view_id), Some(TabSlot::Editable(ed)) if ed.has_pending_native_edit(cx))
                        || snapshot(ws, *target)? != *expected
                    {
                        return Err("rendered readonly editor accepted native text mutation".into());
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    dispatch_dialog(window, cx, false)?;
    wait_until(
        Duration::from_secs(5),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).is_lifecycle_busy()),
        "aggregate close cancellation",
    )
    .await?;
    pump_frame(window, cx)?;
    window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                for target in &targets {
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                        return Err("cancel lost a view".into());
                    };
                    if ed.editor().read(cx).presentation().is_readonly() {
                        return Err("cancel failed to restore original readonly".into());
                    }
                }
                for (target, expected) in targets.iter().zip(&before) {
                    if snapshot(ws, *target)? != *expected {
                        return Err("cancel changed protected draft state".into());
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    report.group_cancel_restores_editability_verified = true;

    // Queue an actual native edit and request close within the same UI turn. The
    // adapter must not capture revision zero/old committed text as a frozen savepoint.
    let guarded_before = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<Revision, String> {
                ws.activate_view(targets[0], window, cx);
                let Some(TabSlot::Editable(ed)) = ws.view_slot(targets[0].view_id) else {
                    return Err("missing guarded editor".into());
                };
                let revision = ed.visible_revision;
                let editor = ed.editor().clone();
                editor.update(cx, |state, cx| {
                    state.replace_text_in_range(Some(0..0), "guarded ", window, cx)
                });
                ws.request_close_group(group, window, cx);
                if ws.pending_lifecycle_sequence().is_some() {
                    return Err("queued native edit was frozen against stale committed text".into());
                }
                if !ws.session_layout().views.contains_key(&targets[0].view_id) {
                    return Err("native pending guard lost its view".into());
                }
                Ok(revision)
            })
        })
        .map_err(|e| e.to_string())??;
    wait_until(Duration::from_secs(10),cx,|cx|cx.update(|cx|matches!(workspace.read(cx).view_slot(targets[0].view_id),Some(TabSlot::Editable(ed)) if ed.visible_revision==Revision(guarded_before.0+1)&&ed.committed_rope().to_string().starts_with("guarded "))),"retained queued native edit").await?;
    report.group_native_pending_guard_verified = true;
    // Register the activated editor's focus node in the real dispatch tree,
    // without splitting the native-edit/close guard's same-turn preflight.
    pump_frame(window, cx)?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(targets[0].view_id) {
                    ed.undo(window, cx);
                }
            })
        })
        .map_err(|e| e.to_string())?;
    wait_until(Duration::from_secs(10),cx,|cx|cx.update(|cx|matches!(workspace.read(cx).view_slot(targets[0].view_id),Some(TabSlot::Editable(ed)) if ed.committed_rope().to_string()==samples[0])),"undo preserved across group operations").await?;
    pump_frame(window, cx)?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.view_slot(targets[0].view_id) {
                    ed.redo(window, cx);
                }
            })
        })
        .map_err(|e| e.to_string())?;
    wait_until(Duration::from_secs(10),cx,|cx|cx.update(|cx|matches!(workspace.read(cx).view_slot(targets[0].view_id),Some(TabSlot::Editable(ed)) if ed.committed_rope().to_string().starts_with("guarded "))),"redo preserved across group operations").await?;
    report.group_undo_preserved_verified = true;
    wait_until(Duration::from_secs(15),cx,|cx|cx.update(|cx|matches!(workspace.read(cx).view_slot(targets[0].view_id),Some(TabSlot::Editable(ed)) if ed.durable_revision==ed.visible_revision)),"guarded draft durable before Keep close").await?;

    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.request_close_group(group, window, cx))
        })
        .map_err(|e| e.to_string())?;
    dispatch_dialog(window, cx, true)?;
    wait_until(
        Duration::from_secs(15),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy()
                    && !ws.session_layout().views.contains_key(&targets[0].view_id)
                    && !ws.session_layout().views.contains_key(&targets[2].view_id)
            })
        },
        "atomic close of all group members",
    )
    .await?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.session_layout().group(group).is_none()
            || ws
                .session_layout()
                .views
                .values()
                .any(|view| view.group_id == group)
        {
            return Err("Close Group removed the group or refilled it with a scratch".into());
        }
        if !ws.session_layout().views.contains_key(&targets[1].view_id) {
            return Err("Close Group affected a different group".into());
        }
        Ok(())
    })?;
    for _ in 0..2 {
        let previous = cx.update(|cx| workspace.read(cx).session_layout().views.len());
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| ws.reopen_recently_closed(window, cx))
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(15),
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    ws.session_layout().views.len() == previous + 1
                        && matches!(ws.active_slot(), Some(TabSlot::Editable(_)))
                })
            },
            "reopened preserved grouped draft",
        )
        .await?;
    }
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                if ws.apply_view_atomic_edits(
                    targets[0],
                    guarded_before,
                    &[(0..0, "stale".into())],
                    window,
                    cx,
                ) != Err(ApplyEditsError::ReadOnly)
                {
                    return Err("obsolete closed ViewId accepted an edit after reopen".into());
                }
                for old in [targets[0], targets[2]] {
                    let view = ws
                        .session_layout()
                        .views
                        .values()
                        .find(|view| view.document == old.document)
                        .ok_or("reopened document missing")?;
                    if view.id == old.view_id || view.group_id != group {
                        return Err(
                            "reopen did not allocate a fresh view in its preserved group".into(),
                        );
                    }
                }
                ws.capture_session_view_states(cx);
                ws.persist_session_layout(cx);
                report.expected_session_layout = Some(ws.session_layout().clone());
                for target in &targets {
                    let view = ws
                        .session_layout()
                        .views
                        .values()
                        .find(|view| view.document == target.document)
                        .ok_or("draft missing before shutdown")?;
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(view.id) else {
                        return Err("reopened draft not hydrated".into());
                    };
                    report.expected_group_drafts.push((
                        target.document,
                        ContentIdentity::of_rope(ed.committed_rope()),
                        ed.visible_revision,
                    ));
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    report.group_close_reopen_verified = true;
    Ok(())
}

pub(crate) fn verify_post_shutdown_layout(
    data_dir: &Path,
    report: &SmokeReport,
) -> Result<(), String> {
    let expected = report
        .expected_session_layout
        .as_ref()
        .ok_or("missing actual native layout snapshot")?;
    let handle =
        RecoveryHandle::start(data_dir.join("recovery.sqlite3")).map_err(|e| e.to_string())?;
    let result = (|| {
        let manifest = handle
            .request_manifest()
            .map_err(|e| e.to_string())?
            .recv_timeout(Duration::from_secs(10))
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        if manifest.layout != *expected {
            return Err("complete restart layout differs in pane tree/ratios/focus, group metadata/order/membership, or view identity/selection/positions".into());
        }
        if manifest.documents.len() != expected.views.len() {
            return Err("restart document/layout bijection differs".into());
        }
        if let Some(bulk) = &report.expected_bulk_group {
            if !report.group_bulk_cancel_verified
                || !report.group_bulk_keep_verified
                || expected.views.len() != 1
                || expected.target(bulk.scratch.view_id) != Some(bulk.scratch)
            {
                return Err("bulk shutdown lacks its complete native Cancel/Keep evidence".into());
            }
            verify_bulk_catalog(
                &handle,
                bulk.group,
                bulk.pane,
                &bulk.drafts,
                Some(bulk.scratch),
            )?;
            verify_bulk_active_content(&handle, bulk)?;
        }
        for (key, identity, revision) in &report.expected_group_drafts {
            let stub = manifest
                .documents
                .iter()
                .find(|stub| stub.key() == *key)
                .ok_or("group draft missing after restart")?;
            if !stub.has_checkpoint || stub.durable_rev < *revision {
                return Err("group draft lacks its expected durable revision".into());
            }
            let draft = handle
                .request_hydrate(*key, CancelToken::new())
                .map_err(|e| e.to_string())?
                .recv_timeout(Duration::from_secs(10))
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            if draft.identity != *identity {
                return Err("group draft content changed across shutdown/restart".into());
            }
        }
        Ok(())
    })();
    let shutdown = handle
        .shutdown()
        .map_err(|e| e.to_string())?
        .recv_timeout(Duration::from_secs(10))
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string());
    result.and(shutdown)
}
