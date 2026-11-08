use super::*;
use editor_services::RecoveryEntryOrigin;
use editor_services::{
    CloseAck, CloseBatch, CloseDisposition, CloseEntry, ClosedViewPage, DocumentChange,
    DocumentMetaRecord, GroupCommand, GroupCommandResult, LayoutAck, LayoutPayload, LayoutRevision,
    LayoutSeq, LayoutView, MAX_SESSION_VIEWS, MAX_SPLIT_RATIO_BPS, MIN_SPLIT_RATIO_BPS,
    PaneCommand, PaneCommandResult, PaneId, RecoveryCatalogSummary, RecoveryCursor,
    RecoveryEntryKey, RecoveryPage, RecoveryPageRequest, ReloadReplacement, Reply, SessionWrite,
    SplitNodeId, save_document_copy,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;

#[derive(Default)]
pub(super) struct PaneRuntime {
    focus_only: bool,
    compact: bool,
    resize: Option<PaneResize>,
}
#[derive(Clone, Copy)]
struct PaneResize {
    split: SplitNodeId,
    original_ratio: u16,
    preview_ratio: u16,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RecoveryBrowserState {
    pub(crate) request_seq: u64,
    pub(crate) loading: bool,
    pub(crate) latest_summary: RecoveryCatalogSummary,
    pub(crate) deferred_on_restore: u64,
    pub(crate) page: Option<RecoveryPage>,
    pub(crate) error: Option<ServiceError>,
    pub(crate) active_export: Option<RecoveryEntryKey>,
}
#[derive(Clone, Copy)]
pub(super) enum PendingDocChangeKind {
    Create,
    Update,
    Reopen,
}
#[derive(Default)]
pub(super) struct SessionPersistence {
    pub(super) revision: Option<LayoutRevision>,
    sequence: u64,
    inflight: Option<LayoutFlight>,
    pub(super) metadata: HashMap<DocumentId, PendingDocChangeKind>,
    metadata_after_reopen: std::collections::HashSet<DocumentId>,
    structural: bool,
    preserve_source: Option<LayoutRevision>,
    failed: bool,
}
struct LayoutFlight {
    sequence: LayoutSeq,
    expected_revision: LayoutRevision,
    reply: Reply<LayoutAck>,
    layout: LayoutPayload,
    intents: HashMap<DocumentId, PendingDocChangeKind>,
    preserve_source: Option<LayoutRevision>,
    started: Instant,
    disconnected: bool,
    original_readonly: Option<Vec<(DocumentId, bool)>>,
    inspection_started: bool,
    inspection: Option<Result<(SessionManifest, ClosedViewPage), ServiceError>>,
}
impl SessionPersistence {
    pub(super) fn allows_commit(&self, id: DocumentId) -> bool {
        !matches!(self.metadata.get(&id), Some(PendingDocChangeKind::Reopen))
            && self.inflight.as_ref().is_none_or(|flight| {
                !matches!(flight.intents.get(&id), Some(PendingDocChangeKind::Reopen))
            })
    }
}
pub(super) enum DeferredCompletion {
    Hydrated {
        target: ViewTarget,
        seq: u64,
        result: Result<HydratedDraft, ServiceError>,
    },
    Disk {
        target: ViewTarget,
        seq: u64,
        path: PathBuf,
        result: Result<OpenOutcome, ServiceError>,
        mru: bool,
    },
    Viewport {
        target: ViewTarget,
        seq: u64,
        result: Result<editor_services::ViewportSlice, ServiceError>,
    },
}
struct FrozenView {
    target: ViewTarget,
    revision: Option<Revision>,
    text_seq: Option<u64>,
    format: (EncodingSpec, LineEnding),
    protected: bool,
    oversized: bool,
    identity: Option<ContentIdentity>,
    view: ViewState,
    disposition: CloseDisposition,
    saved_in_batch: bool,
}
enum LifecycleKind {
    Close,
    Reload {
        path: PathBuf,
        fresh: DocumentKey,
        staged: Option<StagedDisk>,
        record_mru: bool,
    },
}
enum LifecyclePhase {
    Confirming,
    Preparing,
    Saving,
    Ready,
    Closing {
        reply: Reply<CloseAck>,
        after: SessionLayout,
        scratch: Option<SessionDocumentStub>,
    },
    Replacing {
        reply: Reply<LayoutAck>,
        after: SessionLayout,
    },
}
pub(super) struct PendingLifecycle {
    sequence: u64,
    started: Instant,
    kind: LifecycleKind,
    phase: LifecyclePhase,
    frozen: Vec<FrozenView>,
    original_readonly: Vec<(DocumentId, bool)>,
    dialog_dropped: Arc<AtomicBool>,
    cancel: CancelToken,
    disconnected: bool,
    reconciliation_started: bool,
    reconciliation: Option<Result<(SessionManifest, ClosedViewPage), ServiceError>>,
    accepted_revision: Option<LayoutRevision>,
    admitted_sequence: Option<LayoutSeq>,
}
struct StagedDisk {
    outcome: OpenOutcome,
    meta: DocMeta,
}
struct DialogLifetime(Arc<AtomicBool>);
impl Drop for DialogLifetime {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn failure(kind: ErrorKind, message: impl Into<String>) -> ServiceError {
    ServiceError::new(kind, message)
}
fn disconnected() -> ServiceError {
    failure(
        ErrorKind::Storage,
        "Recovery worker disconnected; outcome is indeterminate, draft retained",
    )
}

impl EditorWorkspace {
    pub(crate) fn session_layout(&self) -> &SessionLayout {
        &self.session
    }
    pub(crate) fn active_pane_id(&self) -> PaneId {
        self.session.active_pane_id()
    }
    pub(crate) fn pane_view_ids(&self, pane: PaneId) -> Option<&[ViewId]> {
        self.session.pane(pane).map(|pane| pane.tabs.as_slice())
    }
    pub(crate) fn pane_active_target(&self, pane: PaneId) -> Option<ViewTarget> {
        self.session
            .pane(pane)?
            .active_view
            .and_then(|id| self.view_target(id))
    }
    pub(crate) fn all_view_ids(&self) -> impl Iterator<Item = ViewId> {
        self.session.view_ids()
    }
    pub(crate) fn visible_pane_ids(&self) -> impl Iterator<Item = PaneId> {
        self.session
            .panes()
            .map(|pane| pane.id)
            .filter(move |pane| self.is_pane_visible(*pane))
    }
    pub(crate) fn visible_targets(&self) -> impl Iterator<Item = ViewTarget> {
        self.visible_pane_ids()
            .filter_map(move |pane| self.pane_active_target(pane))
    }
    pub(crate) fn is_pane_visible(&self, pane: PaneId) -> bool {
        self.session.pane(pane).is_some()
            && (!(self.pane_runtime.focus_only || self.pane_runtime.compact)
                || pane == self.active_pane_id())
    }
    pub(crate) fn is_focus_only(&self) -> bool {
        self.pane_runtime.focus_only
    }
    pub(crate) fn is_compact_layout(&self) -> bool {
        self.pane_runtime.compact
    }
    pub(crate) fn active_view_id(&self) -> Option<ViewId> {
        self.session.active_view_id()
    }
    pub(crate) fn view_target(&self, view: ViewId) -> Option<ViewTarget> {
        self.session.target(view)
    }
    pub(crate) fn view_slot(&self, view: ViewId) -> Option<&TabSlot> {
        let key = self.session.view(view)?.document;
        self.documents
            .get(&key.doc_id)
            .filter(|s| s.document_key() == key)
    }
    pub(crate) fn active_slot(&self) -> Option<&TabSlot> {
        self.view_slot(self.active_view_id()?)
    }
    pub(super) fn active_document_id(&self) -> Option<DocumentId> {
        self.active_slot().map(TabSlot::doc_id)
    }
    pub(super) fn active_slot_mut(&mut self) -> Option<&mut TabSlot> {
        let id = self.active_document_id()?;
        self.documents.get_mut(&id)
    }
    pub(crate) fn active_editable_mut(&mut self) -> Option<&mut EditorAdapter> {
        if self.is_lifecycle_busy() {
            return None;
        }
        match self.active_slot_mut()? {
            TabSlot::Editable(ed) => Some(ed),
            _ => None,
        }
    }
    pub(crate) fn recovery_browser_state(&self) -> &RecoveryBrowserState {
        &self.recovery_browser
    }
    pub(crate) fn recently_closed_count(&self) -> usize {
        self.recently_closed.len()
    }
    pub(crate) fn is_lifecycle_busy(&self) -> bool {
        self.bootstrap_indeterminate
            || self.pending_lifecycle.is_some()
            || self.shutting_down
            || self
                .persistence
                .inflight
                .as_ref()
                .is_some_and(|flight| flight.disconnected)
    }
    pub(crate) fn pending_lifecycle_sequence(&self) -> Option<u64> {
        self.pending_lifecycle.as_ref().map(|p| p.sequence)
    }
    pub(crate) fn layout_write_count(&self) -> u64 {
        self.persistence.sequence
    }
    pub(crate) fn palette_origin(&self) -> Option<OverlayOrigin> {
        self.palette_prior_origin
    }
    pub(crate) fn capture_overlay_origin(&self, window: &Window, cx: &App) -> OverlayOrigin {
        let focus = window.focused(cx);
        let focus = if focus.as_ref() == Some(&self.search_query_input.read(cx).focus_handle(cx)) {
            OverlayFocus::SearchQuery
        } else if focus.as_ref() == Some(&self.search_replace_input.read(cx).focus_handle(cx)) {
            OverlayFocus::SearchReplace
        } else if matches!(self.active_slot(), Some(TabSlot::Editable(ed)) if focus.as_ref() == Some(&ed.editor().read(cx).focus_handle(cx)))
        {
            OverlayFocus::Editor
        } else {
            OverlayFocus::Workspace
        };
        OverlayOrigin {
            pane: self.active_pane_id(),
            view: self.active_view_id().and_then(|id| self.view_target(id)),
            focus,
        }
    }
    pub(crate) fn restore_overlay_origin(
        &mut self,
        origin: OverlayOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_lifecycle_busy() {
            if let Some(target) = origin
                .view
                .filter(|target| self.session.resolve(*target).is_ok())
            {
                if self.active_view_id() != Some(target.view_id) {
                    self.activate_view(target, window, cx);
                }
            } else if self.session.pane(origin.pane).is_some()
                && self.active_pane_id() != origin.pane
            {
                let _ = self.activate_pane(origin.pane, window, cx);
            }
        }
        match origin.focus {
            OverlayFocus::SearchQuery if self.search.visible => self
                .search_query_input
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx),
            OverlayFocus::SearchReplace if self.search.visible && self.search.replace_visible => {
                self.search_replace_input
                    .read(cx)
                    .focus_handle(cx)
                    .focus(window, cx)
            }
            _ => self.focus_active(window, cx),
        }
    }
    pub(super) fn notify_error(
        &self,
        error: ServiceError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::warning(error.message), cx);
    }
    fn resolve_slot(&self, target: ViewTarget) -> Result<&TabSlot, ServiceError> {
        self.session.resolve(target)?;
        self.documents
            .get(&target.document.doc_id)
            .filter(|s| s.document_key() == target.document)
            .ok_or_else(|| failure(ErrorKind::Stale, "Document identity changed"))
    }
    pub(super) fn mutation_allowed(&self) -> bool {
        !self.is_lifecycle_busy()
    }
    pub(super) fn adopt_read_only_recovery(&mut self) {
        self.recovery = None;
        self.recovery_read_only = true;
        self.persistence.failed = true;
    }
    fn next_operation(&mut self) -> Result<u64, ServiceError> {
        self.operation_sequence = self
            .operation_sequence
            .checked_add(1)
            .ok_or_else(|| failure(ErrorKind::Storage, "Operation sequence exhausted"))?;
        Ok(self.operation_sequence)
    }
    fn next_layout_sequence(&mut self) -> Result<LayoutSeq, ServiceError> {
        self.persistence.sequence = self
            .persistence
            .sequence
            .checked_add(1)
            .ok_or_else(|| failure(ErrorKind::Storage, "Layout sequence exhausted"))?;
        Ok(LayoutSeq(self.persistence.sequence))
    }
    pub(super) fn mark_metadata(&mut self, id: DocumentId, kind: PendingDocChangeKind) {
        if matches!(kind, PendingDocChangeKind::Update)
            && matches!(
                self.persistence.metadata.get(&id),
                Some(PendingDocChangeKind::Reopen)
            )
        {
            self.persistence.metadata_after_reopen.insert(id);
        }
        self.persistence
            .metadata
            .entry(id)
            .and_modify(|old| {
                if !matches!(
                    old,
                    PendingDocChangeKind::Create | PendingDocChangeKind::Reopen
                ) {
                    *old = kind;
                }
            })
            .or_insert(kind);
        self.persistence.structural = true;
        self.layout_dirty = true;
    }
    pub(super) fn document_commit_admissible(&self, id: DocumentId) -> bool {
        self.persistence.allows_commit(id)
    }
    fn capture_view(&mut self, id: ViewId, cx: &App) {
        let view = match self.view_slot(id) {
            Some(TabSlot::Editable(ed)) => ed.capture_view_state(cx),
            Some(TabSlot::Paged(pv)) => pv.capture_view_state(),
            _ => return,
        };
        if let Some(record) = self.session.views.get_mut(&id) {
            if record.view != view {
                record.view = view;
                self.persistence.structural = true;
                self.layout_dirty = true;
            }
        }
    }
    pub(crate) fn capture_session_view_states(&mut self, cx: &App) {
        let views = self.all_view_ids().collect::<Vec<_>>();
        for id in views {
            self.capture_view(id, cx);
        }
    }
    pub(crate) fn persist_session_layout(&mut self, cx: &App) {
        if let Some(id) = self.active_view_id() {
            self.capture_view(id, cx);
        }
        self.layout_dirty = true;
    }
    fn admit_layout(&mut self) -> Result<(), ServiceError> {
        if self.persistence.inflight.is_some() || !self.layout_dirty {
            return Ok(());
        }
        let revision = self
            .persistence
            .revision
            .ok_or_else(|| failure(ErrorKind::Stale, "Session manifest has not been adopted"))?;
        let recovery = self
            .recovery
            .clone()
            .ok_or_else(|| failure(ErrorKind::Storage, "Recovery storage is unavailable"))?;
        let sequence = self.next_layout_sequence()?;
        let changes = self
            .persistence
            .metadata
            .iter()
            .filter_map(|(id, kind)| {
                let slot = self.documents.get(id)?;
                Some(match kind {
                    PendingDocChangeKind::Create => DocumentChange::Create(DocumentMetaRecord {
                        key: slot.document_key(),
                        meta: slot.meta().clone(),
                    }),
                    PendingDocChangeKind::Update => DocumentChange::Update(DocumentMetaRecord {
                        key: slot.document_key(),
                        meta: slot.meta().clone(),
                    }),
                    PendingDocChangeKind::Reopen => DocumentChange::Reopen(slot.document_key()),
                })
            })
            .collect::<Vec<_>>();
        let layout = if self.persistence.structural || !changes.is_empty() {
            LayoutPayload::Structural(self.session.clone())
        } else {
            LayoutPayload::TreeOnly(self.session.tree.clone())
        };
        let reply = recovery.write_session(SessionWrite {
            sequence,
            expected_revision: revision,
            layout: layout.clone(),
            document_changes: changes,
            preserve_source_layout: self.persistence.preserve_source,
        })?;
        self.persistence.inflight = Some(LayoutFlight {
            sequence,
            expected_revision: revision,
            reply,
            layout,
            intents: std::mem::take(&mut self.persistence.metadata),
            preserve_source: self.persistence.preserve_source.take(),
            started: Instant::now(),
            disconnected: false,
            original_readonly: None,
            inspection_started: false,
            inspection: None,
        });
        self.persistence.structural = false;
        self.layout_dirty = false;
        Ok(())
    }
    pub(crate) fn apply_group_command(
        &mut self,
        command: GroupCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<GroupCommandResult, ServiceError> {
        if self.is_lifecycle_busy() {
            return Err(failure(
                ErrorKind::Busy,
                "A document lifecycle operation is pending",
            ));
        }
        if self.recovery.is_none() || self.persistence.revision.is_none() || self.persistence.failed
        {
            return Err(failure(
                ErrorKind::Storage,
                "Groups require available session persistence",
            ));
        }
        let structural = matches!(
            command,
            GroupCommand::Create {
                initial_view: Some(_),
                ..
            } | GroupCommand::MoveView { .. }
                | GroupCommand::Remove { .. }
        );
        let result = self.session.apply_group_command(command)?;
        if result.changed {
            self.persistence.structural |= structural;
            self.layout_dirty = true;
            cx.notify();
        }
        Ok(result)
    }
    pub fn hydrated_editor_count(&self) -> usize {
        self.documents
            .values()
            .filter(|s| matches!(s, TabSlot::Editable(_)))
            .count()
    }
    pub fn hydrated_content_bytes(&self) -> u64 {
        self.documents
            .values()
            .filter_map(|s| match s {
                TabSlot::Editable(ed) => Some(ed.committed_byte_len()),
                _ => None,
            })
            .sum()
    }
    fn collect_hydrated_slots(&self, cx: &App) -> Vec<HydratedSlotInfo> {
        self.documents
            .values()
            .filter_map(|s| match s {
                TabSlot::Editable(ed) => Some(HydratedSlotInfo {
                    doc_id: ed.doc_id,
                    byte_len: ed
                        .committed_byte_len()
                        .max(ed.editor().read(cx).text().len() as u64),
                    is_dirty: ed.is_dirty() || ed.has_pending_native_edit(cx),
                    has_undo_history: ed.has_undo_history(),
                    is_file_backed: ed.meta.path.is_some(),
                    is_visible: self
                        .visible_targets()
                        .any(|target| target.document.doc_id == ed.doc_id),
                    last_activated_tick: ed.last_activated_tick,
                }),
                _ => None,
            })
            .collect()
    }
    fn dehydrate_clean_documents(&mut self, ids: &[DocumentId], cx: &mut Context<Self>) {
        for id in ids {
            if self
                .visible_targets()
                .any(|target| target.document.doc_id == *id)
            {
                continue;
            }
            let Some(TabSlot::Editable(ed)) = self.documents.get(id) else {
                continue;
            };
            if ed.is_dirty()
                || ed.has_undo_history()
                || ed.meta.path.is_none()
                || ed.has_pending_native_edit(cx)
            {
                continue;
            }
            let stub = SessionDocumentStub {
                doc_id: *id,
                lifecycle_gen: ed.lifecycle_gen,
                durable_rev: ed.durable_revision,
                meta: ed.meta.clone(),
                has_checkpoint: ed.has_checkpoint,
            };
            if let Some(view) = self
                .session
                .views
                .values()
                .find(|v| v.document.doc_id == *id)
                .map(|v| v.id)
            {
                self.capture_view(view, cx);
            }
            self.documents.insert(*id, TabSlot::Dehydrated(stub));
        }
    }
    fn make_adapter(
        &mut self,
        target: ViewTarget,
        stub: SessionDocumentStub,
        rope: Rope,
        clean: bool,
        conflict: bool,
        view: ViewState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> EditorAdapter {
        self.activation_clock = self.activation_clock.saturating_add(1);
        let mut adapter = EditorAdapter::new(
            HydrateEditorParams {
                doc_id: stub.doc_id,
                lifecycle_gen: stub.lifecycle_gen,
                meta: stub.meta,
                rope,
                initial_rev: stub.durable_rev,
                durable_rev: stub.durable_rev,
                is_clean_at_load: clean,
                disk_conflict: conflict,
                initial_view: view,
                initial_prefs: self.current_view_prefs(),
                activation_tick: self.activation_clock,
            },
            window,
            cx,
        );
        adapter.has_checkpoint = stub.has_checkpoint;
        adapter.attach_subscription(self.subscribe_editor(&adapter, target, window, cx));
        adapter
    }
    pub(crate) fn create_untitled_tab(
        &mut self,
        persist_layout: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ViewTarget> {
        if !self.mutation_allowed() || self.session.views.len() >= MAX_SESSION_VIEWS {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "Cannot add a view during lifecycle work or beyond the 1024-view capacity",
                ),
                window,
                cx,
            );
            return None;
        }
        self.untitled_counter += 1;
        let stub = SessionDocumentStub {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
            durable_rev: Revision(0),
            meta: DocMeta {
                title: format!("untitled-{}", self.untitled_counter),
                ..DocMeta::default()
            },
            has_checkpoint: false,
        };
        let id = ViewId::new();
        let target = ViewTarget {
            view_id: id,
            document: stub.key(),
        };
        let slot =
            match plan_hydration_admission(self.budget_config, &self.collect_hydrated_slots(cx), 0)
            {
                AdmissionDecision::Admit {
                    dehydrate_documents,
                } => {
                    self.dehydrate_clean_documents(&dehydrate_documents, cx);
                    TabSlot::Editable(self.make_adapter(
                        target,
                        stub,
                        Rope::new(),
                        true,
                        false,
                        ViewState::default(),
                        window,
                        cx,
                    ))
                }
                AdmissionDecision::OverBudget { .. } => TabSlot::Dehydrated(stub),
            };
        self.session
            .insert_view(
                self.active_pane_id(),
                LayoutView {
                    id,
                    document: target.document,
                    group_id: self.session.active_group(),
                    view: ViewState::default(),
                },
                None,
                false,
            )
            .ok()?;
        self.documents.insert(target.document.doc_id, slot);
        self.mark_metadata(target.document.doc_id, PendingDocChangeKind::Create);
        self.activate_view(target, window, cx);
        if persist_layout {
            self.persist_session_layout(cx);
        }
        cx.notify();
        Some(target)
    }
    pub(super) fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(TabSlot::Editable(ed)) = self.active_slot() {
            ed.focus(window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
    }
    pub(crate) fn activate_view(
        &mut self,
        target: ViewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = (|| {
            self.resolve_slot(target)?;
            let pane = self
                .session
                .pane_for_view(target.view_id)
                .ok_or_else(|| failure(ErrorKind::Stale, "View is no longer placed"))?;
            self.apply_pane_command(
                PaneCommand::Activate {
                    pane,
                    view: Some(target),
                },
                window,
                cx,
            )?;
            Ok::<_, ServiceError>(())
        })();
        if let Err(error) = result {
            self.notify_error(error, window, cx);
        }
    }
    pub(crate) fn activate_pane(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        self.apply_pane_command(PaneCommand::Activate { pane, view: None }, window, cx)
            .map(|_| ())
    }
    pub(crate) fn apply_pane_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<PaneCommandResult, ServiceError> {
        if self.is_lifecycle_busy() {
            return Err(failure(
                ErrorKind::Busy,
                "Selection and layout are frozen during lifecycle work",
            ));
        }
        let activation = matches!(command, PaneCommand::Activate { .. });
        let ratio_only = matches!(
            command,
            PaneCommand::SetRatio { .. } | PaneCommand::Equalize
        );
        if !activation
            && (self.recovery.is_none()
                || self.persistence.revision.is_none()
                || self.persistence.failed)
        {
            return Err(failure(
                ErrorKind::Storage,
                "Pane changes require available session persistence",
            ));
        }
        let old = self.active_view_id().and_then(|id| self.view_target(id));
        let result = self.session.apply_pane_command(command)?;
        if result.changed {
            self.pane_runtime.resize = None;
            self.persistence.structural |= result.views_changed;
            self.layout_dirty = true;
        }
        if activation || result.changed {
            if !ratio_only {
                self.finish_target_switch(old, window, cx);
            }
            cx.notify();
        }
        Ok(result)
    }
    pub(super) fn clear_target_highlights(
        &mut self,
        target: Option<ViewTarget>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = target else { return };
        if self.session.resolve(target).is_err() {
            return;
        }
        match self.documents.get_mut(&target.document.doc_id) {
            Some(TabSlot::Editable(ed)) => ed.clear_search_highlights(cx),
            Some(TabSlot::Paged(pv)) => pv.highlighted_byte_range = None,
            _ => {}
        }
    }
    fn finish_target_switch(
        &mut self,
        old: Option<ViewTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.active_view_id().and_then(|id| self.view_target(id));
        if old != next {
            if let Some(old) = old {
                self.capture_view(old.view_id, cx);
                self.flush_document_commit(old.document.doc_id);
            }
            self.clear_target_highlights(old, cx);
            self.clear_target_highlights(self.search.target_view, cx);
            self.search.cancel_active();
            self.search.clear_results();
            self.search.target_view = None;
            self.rebuild_search_item_sizes();
        }
        let prefs = self.current_view_prefs();
        self.activation_clock = self.activation_clock.saturating_add(1);
        if let Some(target) = next {
            if let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&target.document.doc_id) {
                ed.last_activated_tick = self.activation_clock;
                ed.apply_view_prefs(prefs, window, cx);
            }
        }
        self.focus_active(window, cx);
        self.hydrate_visible_panes(window, cx);
        if old != next && self.search.visible {
            self.run_search_from_inputs(window, cx);
        }
    }
    pub(crate) fn cycle_pane(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let count = self.session.panes().count();
        let position = self
            .session
            .panes()
            .position(|p| p.id == self.active_pane_id())
            .unwrap();
        let next = if forward {
            (position + 1) % count
        } else {
            (position + count - 1) % count
        };
        let pane = self.session.panes().nth(next).unwrap().id;
        self.activate_pane(pane, window, cx)
    }
    pub(crate) fn set_focus_only(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_runtime.focus_only != enabled {
            self.pane_runtime.focus_only = enabled;
            self.pane_runtime.resize = None;
            self.apply_view_preferences_to_visible(window, cx);
            self.hydrate_visible_panes(window, cx);
            cx.notify();
        }
    }
    pub(crate) fn set_compact_layout(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pane_runtime.compact != enabled {
            self.pane_runtime.compact = enabled;
            self.pane_runtime.resize = None;
            self.apply_view_preferences_to_visible(window, cx);
            self.hydrate_visible_panes(window, cx);
            cx.notify();
        }
    }
    pub(crate) fn begin_pane_resize(
        &mut self,
        split: SplitNodeId,
        _cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        if self.is_lifecycle_busy() {
            return Err(failure(ErrorKind::Busy, "Layout is frozen"));
        }
        if self.recovery.is_none() || self.persistence.revision.is_none() || self.persistence.failed
        {
            return Err(failure(
                ErrorKind::Storage,
                "Pane changes require available session persistence",
            ));
        }
        let ratio = self
            .session
            .split_ratio(split)
            .ok_or_else(|| failure(ErrorKind::Stale, "Split no longer exists"))?;
        self.pane_runtime.resize = Some(PaneResize {
            split,
            original_ratio: ratio,
            preview_ratio: ratio,
        });
        Ok(())
    }
    pub(crate) fn update_pane_resize(
        &mut self,
        split: SplitNodeId,
        ratio_bps: u16,
        cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        if self.is_lifecycle_busy() {
            self.pane_runtime.resize = None;
            return Err(failure(ErrorKind::Busy, "Layout is frozen"));
        }
        if !(MIN_SPLIT_RATIO_BPS..=MAX_SPLIT_RATIO_BPS).contains(&ratio_bps) {
            return Err(failure(ErrorKind::InvalidInput, "Split ratio out of range"));
        }
        let resize = self
            .pane_runtime
            .resize
            .as_mut()
            .filter(|r| r.split == split)
            .ok_or_else(|| failure(ErrorKind::Stale, "Resize gesture no longer exists"))?;
        if self.session.split_ratio(split) != Some(resize.original_ratio) {
            self.pane_runtime.resize = None;
            return Err(failure(ErrorKind::Stale, "Split changed during resize"));
        }
        resize.preview_ratio = ratio_bps;
        cx.notify();
        Ok(())
    }
    pub(crate) fn active_pane_resize(&self) -> Option<SplitNodeId> {
        self.pane_runtime.resize.map(|resize| resize.split)
    }
    pub(crate) fn cancel_pane_resize(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pane_runtime.resize.take().is_some() {
            cx.notify();
            true
        } else {
            false
        }
    }
    pub(crate) fn finish_pane_resize(
        &mut self,
        commit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let Some(resize) = self.pane_runtime.resize.take() else {
            return Ok(());
        };
        cx.notify();
        if commit {
            self.apply_pane_command(
                PaneCommand::SetRatio {
                    split: resize.split,
                    expected_ratio_bps: resize.original_ratio,
                    ratio_bps: resize.preview_ratio,
                },
                window,
                cx,
            )?;
        }
        Ok(())
    }
    pub(crate) fn pane_split_ratio(&self, split: SplitNodeId) -> Option<u16> {
        self.pane_runtime
            .resize
            .filter(|resize| resize.split == split)
            .map(|resize| resize.preview_ratio)
            .or_else(|| self.session.split_ratio(split))
    }
    pub(crate) fn hydrate_visible_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_lifecycle_busy() {
            return;
        }
        // One automatic hydration globally; a visibility change cannot admit a
        // second candidate ahead of the already scheduled focused document.
        loop {
            let next = std::iter::once(self.pane_active_target(self.active_pane_id()))
                .flatten()
                .chain(
                    self.visible_targets()
                        .filter(|target| Some(target.view_id) != self.active_view_id()),
                )
                .find(|target| {
                    matches!(
                        self.view_slot(target.view_id),
                        Some(TabSlot::Dehydrated(_) | TabSlot::Hydrating { .. })
                    )
                });
            if !self.hydration_requests.is_empty() {
                break;
            }
            let Some(target) = next else { break };
            if matches!(
                self.view_slot(target.view_id),
                Some(TabSlot::Hydrating { .. })
            ) {
                break;
            }
            self.retry_view_hydration(target, window, cx);
            if matches!(self.view_slot(target.view_id), Some(TabSlot::Dehydrated(_))) {
                break;
            }
        }
        let mut paged = [None; editor_services::MAX_PANES];
        let mut count = 0;
        for target in self.visible_targets() {
            if let Some(TabSlot::Paged(pv)) = self.view_slot(target.view_id)
                && pv.current_slice.is_none()
                && !pv.is_loading
                && pv.source_error.is_none()
            {
                paged[count] = Some((target, pv.current_anchor_byte));
                count += 1;
            }
        }
        for (target, anchor) in paged.into_iter().flatten() {
            self.request_paged_viewport(target, ViewportAnchor::ByteOffset(anchor), window, cx);
        }
    }
    pub(crate) fn request_close_pane_tabs(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.session.pane(pane) else {
            return;
        };
        let targets = pane
            .tabs
            .iter()
            .filter_map(|id| self.view_target(*id))
            .collect::<Vec<_>>();
        if !targets.is_empty() {
            self.request_close_targets(targets, window, cx);
        }
    }
    pub(super) fn flush_document_commit(&mut self, id: DocumentId) {
        if !self.document_commit_admissible(id) {
            return;
        }
        let Some(recovery) = &self.recovery else {
            return;
        };
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&id) else {
            return;
        };
        let Some(commit) = ed.build_recovery_commit() else {
            return;
        };
        let rev = commit.new_rev;
        match recovery.try_commit(commit) {
            Ok(()) => ed.record_commit_queued(rev),
            Err(e) if e.kind == ErrorKind::Busy => ed.record_commit_busy(),
            Err(e) => self.recovery_degraded_error = Some(e.message),
        }
    }
    pub(crate) fn apply_view_atomic_edits(
        &mut self,
        target: ViewTarget,
        expected_revision: Revision,
        edits: &[(std::ops::Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Revision, ApplyEditsError> {
        if !self.mutation_allowed() || self.session.resolve(target).is_err() {
            return Err(ApplyEditsError::ReadOnly);
        }
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&target.document.doc_id) else {
            return Err(ApplyEditsError::ReadOnly);
        };
        let (rev, outcome) = ed.apply_edits(expected_revision, edits, window, cx)?;
        if let Some(outcome) = outcome {
            self.finish_committed_mutation(target.document.doc_id, outcome, cx);
        }
        Ok(rev)
    }
    fn cancel_hydration(&mut self, target: ViewTarget) {
        if let Some((_, cancel)) = self.hydration_requests.remove(&target) {
            cancel.cancel();
        }
    }
    fn begin_hydration(&mut self, target: ViewTarget, sequence: u64) -> CancelToken {
        self.cancel_hydration(target);
        let cancel = CancelToken::new();
        self.hydration_requests
            .insert(target, (sequence, cancel.clone()));
        cancel
    }
    fn finish_hydration(&mut self, target: ViewTarget, sequence: u64) {
        if self
            .hydration_requests
            .get(&target)
            .is_some_and(|(current, _)| *current == sequence)
        {
            self.hydration_requests.remove(&target);
        }
    }
    pub(crate) fn retry_view_hydration(
        &mut self,
        target: ViewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() {
            return;
        }
        let stub = match self.resolve_slot(target) {
            Ok(
                TabSlot::Dehydrated(s)
                | TabSlot::Hydrating { stub: s, .. }
                | TabSlot::OverBudgetDraft { stub: s, .. }
                | TabSlot::Failed { stub: s, .. },
            ) => s.clone(),
            Ok(_) => return,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        let seq = match self.next_operation() {
            Ok(seq) => seq,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        self.cancel_hydration(target);
        self.documents.insert(
            stub.doc_id,
            TabSlot::Hydrating {
                stub: stub.clone(),
                req_seq: seq,
            },
        );
        if stub.has_checkpoint {
            let recovery = self.recovery.clone();
            let database = self.cli.data_dir.join("recovery.sqlite3");
            if recovery.is_none() && !self.recovery_read_only {
                self.documents.insert(
                    stub.doc_id,
                    TabSlot::Failed {
                        stub,
                        error: "Recovery service unavailable; original draft retained".into(),
                    },
                );
                return;
            }
            let cancel = self.begin_hydration(target, seq);
            cx.spawn_in(window, async move |weak, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        hydrate_from_source(recovery.as_ref(), &database, target.document, &cancel)
                    })
                    .await;
                let _ = weak.update_in(cx, |this, window, cx| {
                    this.complete_hydration(target, seq, result, window, cx)
                });
            })
            .detach();
        } else if let Some(path) = stub.meta.path {
            self.spawn_disk_open(target, seq, path, false, false, window, cx);
        } else {
            let view = self.session.views[&target.view_id].view.clone();
            match plan_hydration_admission(self.budget_config, &self.collect_hydrated_slots(cx), 0)
            {
                AdmissionDecision::Admit {
                    dehydrate_documents,
                } => {
                    self.dehydrate_clean_documents(&dehydrate_documents, cx);
                    let adapter =
                        self.make_adapter(target, stub, Rope::new(), true, false, view, window, cx);
                    self.documents
                        .insert(target.document.doc_id, TabSlot::Editable(adapter));
                    if self.active_view_id() == Some(target.view_id) {
                        self.focus_active(window, cx);
                    }
                }
                AdmissionDecision::OverBudget { reason, .. } => {
                    self.documents
                        .insert(stub.doc_id, TabSlot::OverBudgetDraft { stub, reason });
                }
            }
        }
        cx.notify();
    }
    fn complete_hydration(
        &mut self,
        target: ViewTarget,
        seq: u64,
        result: Result<HydratedDraft, ServiceError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy() {
            self.deferred_completions
                .push(DeferredCompletion::Hydrated {
                    target,
                    seq,
                    result,
                });
            return;
        }
        self.finish_hydration(target, seq);
        let stub = match self.resolve_slot(target) {
            Ok(TabSlot::Hydrating { stub, req_seq }) if *req_seq == seq => stub.clone(),
            _ => {
                self.hydrate_visible_panes(window, cx);
                return;
            }
        };
        match result {
            Ok(draft)
                if draft.doc_id == target.document.doc_id
                    && draft.lifecycle_gen == target.document.lifecycle_gen =>
            {
                match plan_hydration_admission(
                    self.budget_config,
                    &self.collect_hydrated_slots(cx),
                    draft.rope.len() as u64,
                ) {
                    AdmissionDecision::Admit {
                        dehydrate_documents,
                    } => {
                        self.dehydrate_clean_documents(&dehydrate_documents, cx);
                        let clean = draft
                            .meta
                            .saved_identity
                            .map_or(draft.rope.len() == 0, |id| id == draft.identity);
                        let view = self.session.views[&target.view_id].view.clone();
                        let adapter = self.make_adapter(
                            target,
                            SessionDocumentStub {
                                meta: draft.meta,
                                durable_rev: draft.durable_rev,
                                ..stub
                            },
                            draft.rope,
                            clean,
                            draft.disk_conflict,
                            view,
                            window,
                            cx,
                        );
                        self.documents
                            .insert(target.document.doc_id, TabSlot::Editable(adapter));
                    }
                    AdmissionDecision::OverBudget { reason, .. } => {
                        self.documents
                            .insert(stub.doc_id, TabSlot::OverBudgetDraft { stub, reason });
                    }
                }
            }
            Ok(_) => {
                self.documents.insert(
                    stub.doc_id,
                    TabSlot::Failed {
                        stub,
                        error: "Hydration returned an obsolete lifecycle".into(),
                    },
                );
            }
            Err(e) => {
                self.documents.insert(
                    stub.doc_id,
                    TabSlot::Failed {
                        stub,
                        error: e.message,
                    },
                );
            }
        }
        if self.active_view_id() == Some(target.view_id) {
            self.focus_active(window, cx);
            if self.search.visible {
                self.run_search_from_inputs(window, cx);
            }
        }
        self.hydrate_visible_panes(window, cx);
        cx.notify();
    }
    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.open_paths(vec![path], window, cx);
    }
    fn sole_untouched_scratch(&self, cx: &App) -> Option<ViewTarget> {
        if self.session.views.len() != 1 || self.session.panes().count() != 1 {
            return None;
        }
        let target = self.active_view_id().and_then(|id| self.view_target(id))?;
        let TabSlot::Editable(ed) = self.resolve_slot(target).ok()? else {
            return None;
        };
        (ed.visible_revision == Revision(0)
            && ed.current_text_seq() == 0
            && ed.committed_byte_len() == 0
            && !ed.is_dirty()
            && !ed.has_checkpoint
            && !ed.has_undo_history()
            && !ed.has_pending_native_edit(cx)
            && ed.meta.path.is_none()
            && ed.meta.encoding == EncodingSpec::Utf8
            && ed.meta.line_ending == LineEnding::Lf
            && ed.meta.language_override.is_none()
            && ed.meta.saved_identity.is_none()
            && ed.meta.origin_fingerprint.is_none()
            && ed.meta.saved_encoding.is_none()
            && ed.meta.saved_line_ending.is_none()
            && ed.meta.readonly_reason.is_none()
            && !ed.meta.paged
            && !self.pending_saves.contains_key(&target))
        .then_some(target)
    }
    fn scratch_is_unpublished(&self, target: ViewTarget) -> bool {
        self.persistence.inflight.is_none()
            && (self.persistence.revision.is_none()
                || matches!(
                    self.persistence.metadata.get(&target.document.doc_id),
                    Some(PendingDocChangeKind::Create)
                ))
    }
    pub(crate) fn open_paths(
        &mut self,
        mut paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() || !self.pending_open_paths.is_empty() {
            self.notify_error(
                failure(ErrorKind::Busy, "Opening is frozen during lifecycle work"),
                window,
                cx,
            );
            return;
        }
        if paths.is_empty() {
            return;
        }
        for path in &mut paths {
            let original = std::mem::take(path);
            *path = std::path::absolute(&original).unwrap_or(original);
        }
        let group = self.session.active_group();
        let replace = self.sole_untouched_scratch(cx).is_some_and(|target| {
            self.scratch_is_unpublished(target)
                || (self.recovery.is_some()
                    && self.persistence.revision.is_some()
                    && !self.persistence.failed)
        });
        let mut available = MAX_SESSION_VIEWS - self.session.views.len() + usize::from(replace);
        let existing = self
            .documents
            .values()
            .filter_map(|slot| slot.meta().path.as_deref())
            .collect::<std::collections::HashSet<_>>();
        let mut seen = std::collections::HashSet::new();
        let mut accepted = VecDeque::new();
        let mut overflow = 0usize;
        for path in &paths {
            if !seen.insert(path.as_path()) {
                continue;
            }
            if existing.contains(path.as_path()) {
                accepted.push_back(path.clone());
            } else if available > 0 {
                available -= 1;
                accepted.push_back(path.clone());
            } else {
                overflow += 1;
            }
        }
        drop(seen);
        drop(existing);
        drop(paths);
        if overflow > 0 {
            self.notify_error(failure(ErrorKind::InvalidInput,format!("Skipped {overflow} file(s): the session has a 1024-view capacity. Close views while keeping their recovery drafts, then open those files again.")),window,cx);
        }
        if accepted.is_empty() {
            return;
        }
        self.pending_open_placement = Some((self.active_pane_id(), group));
        self.pending_open_paths = accepted;
        self.drain_open_paths(window, cx);
    }
    fn drain_open_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while !self.is_lifecycle_busy() {
            let Some(path) = self.pending_open_paths.pop_front() else {
                self.pending_open_placement = None;
                return;
            };
            let (pane, group) = self
                .pending_open_placement
                .expect("accepted open batch owns its invocation pane and group");
            self.open_batch_path(path, pane, group, window, cx);
        }
    }
    fn cancel_open_batch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let remaining = self.pending_open_paths.len();
        self.pending_open_paths.clear();
        self.pending_open_placement = None;
        if remaining > 0 {
            self.notify_error(
                failure(
                    ErrorKind::Cancelled,
                    format!("Cancelled opening {remaining} remaining file(s)"),
                ),
                window,
                cx,
            );
        }
    }
    fn open_batch_path(
        &mut self,
        path: PathBuf,
        pane: PaneId,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.pane(pane).is_none() || self.session.group(group).is_none() {
            self.notify_error(
                failure(ErrorKind::Stale, "Open destination no longer exists"),
                window,
                cx,
            );
            return;
        }
        if let Some(target) = self
            .session
            .views
            .values()
            .find(|view| {
                self.documents
                    .get(&view.document.doc_id)
                    .is_some_and(|slot| slot.meta().path.as_ref() == Some(&path))
            })
            .map(|view| ViewTarget {
                view_id: view.id,
                document: view.document,
            })
        {
            self.activate_view(target, window, cx);
            self.record_recent_file(path, window, cx);
            return;
        }
        let scratch = self.sole_untouched_scratch(cx);
        let unpublished = scratch.filter(|target| self.scratch_is_unpublished(*target));
        if let Some(target) = scratch.filter(|_| {
            unpublished.is_none()
                && self.recovery.is_some()
                && self.persistence.revision.is_some()
                && !self.persistence.failed
        }) {
            let fresh = DocumentKey {
                doc_id: DocumentId::new(),
                lifecycle_gen: LifecycleGen(1),
            };
            match self.freeze_lifecycle(
                vec![target],
                LifecycleKind::Reload {
                    path,
                    fresh,
                    staged: None,
                    record_mru: true,
                },
                cx,
            ) {
                Ok(seq) => self.stage_reload(seq, window, cx),
                Err(error) => self.notify_error(error, window, cx),
            }
            return;
        }
        if self.session.views.len() >= MAX_SESSION_VIEWS && unpublished.is_none() {
            self.notify_error(
                failure(
                    ErrorKind::InvalidInput,
                    "Session has 1024 views; file was not opened",
                ),
                window,
                cx,
            );
            return;
        }
        let seq = match self.next_operation() {
            Ok(seq) => seq,
            Err(error) => {
                self.notify_error(error, window, cx);
                return;
            }
        };
        let key = DocumentKey {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
        };
        let target = ViewTarget {
            view_id: ViewId::new(),
            document: key,
        };
        let stub = SessionDocumentStub {
            doc_id: key.doc_id,
            lifecycle_gen: key.lifecycle_gen,
            durable_rev: Revision(0),
            has_checkpoint: false,
            meta: DocMeta {
                title: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("document")
                    .into(),
                path: Some(path.clone()),
                ..DocMeta::default()
            },
        };
        if let Some(scratch) = unpublished {
            // No SQL row, checkpoint, native edit, undo history, or user customization exists.
            if let Err(error) = self.session.remove_views(&[scratch]) {
                self.notify_error(error, window, cx);
                return;
            }
            self.documents.remove(&scratch.document.doc_id);
            self.persistence.metadata.remove(&scratch.document.doc_id);
            self.persistence
                .metadata_after_reopen
                .remove(&scratch.document.doc_id);
        }
        if let Err(error) = self.session.insert_view(
            pane,
            LayoutView {
                id: target.view_id,
                document: key,
                group_id: group,
                view: ViewState::default(),
            },
            None,
            false,
        ) {
            self.notify_error(error, window, cx);
            return;
        }
        self.documents
            .insert(key.doc_id, TabSlot::Hydrating { stub, req_seq: seq });
        self.mark_metadata(key.doc_id, PendingDocChangeKind::Create);
        self.activate_view(target, window, cx);
        self.spawn_disk_open(target, seq, path, true, false, window, cx);
        cx.notify();
    }
    fn disk_options(&self, exclude: Option<DocumentId>, cx: &App) -> (OpenOptions, bool) {
        let mut options = OpenOptions::default();
        if let Some(limit) = self.cli.editable_byte_limit {
            options.editable_byte_limit = limit;
        }
        let slots = self.collect_hydrated_slots(cx);
        let (used, count) = slots
            .into_iter()
            .filter(|s| Some(s.doc_id) != exclude)
            .filter(|s| s.is_visible || s.is_dirty || s.has_undo_history || !s.is_file_backed)
            .fold((0u64, 0usize), |(bytes, count), slot| {
                (bytes.saturating_add(slot.byte_len), count + 1)
            });
        options.editable_byte_limit = options
            .editable_byte_limit
            .min(self.budget_config.max_hydrated_bytes.saturating_sub(used));
        let force_paged = count >= self.budget_config.max_hydrated_editors;
        if force_paged {
            options.editable_byte_limit = 0;
        }
        (options, force_paged)
    }
    fn spawn_disk_open(
        &mut self,
        target: ViewTarget,
        seq: u64,
        path: PathBuf,
        mru: bool,
        force_paged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paging = self.paging_service.clone();
        let (options, limit_reached) = self.disk_options(None, cx);
        let cancel = self.begin_hydration(target, seq);
        cx.spawn_in(window, async move |weak, cx| {
            let (path, result) = cx
                .background_executor()
                .spawn(async move {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    let result = open_disk_for_budget(
                        target.document.doc_id,
                        &path,
                        &paging,
                        options,
                        force_paged || limit_reached,
                        &cancel,
                    );
                    (path, result)
                })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.complete_disk_open(target, seq, path, result, mru, window, cx)
            });
        })
        .detach();
    }
    fn stage_meta(path: &Path, outcome: OpenOutcome, language: Option<String>) -> StagedDisk {
        let (encoding, line_ending, fingerprint, saved_identity, readonly, paged) = match &outcome {
            OpenOutcome::Editable {
                encoding,
                line_ending,
                fingerprint,
                identity,
                readonly_reason,
                ..
            } => (
                *encoding,
                *line_ending,
                fingerprint.clone(),
                Some(*identity),
                readonly_reason.clone(),
                false,
            ),
            OpenOutcome::Paged {
                encoding,
                line_ending,
                fingerprint,
                readonly_reason,
                ..
            } => (
                *encoding,
                *line_ending,
                fingerprint.clone(),
                None,
                Some(readonly_reason.clone()),
                true,
            ),
        };
        StagedDisk {
            outcome,
            meta: DocMeta {
                title: path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("document")
                    .into(),
                path: Some(path.to_path_buf()),
                encoding,
                line_ending,
                origin_fingerprint: Some(fingerprint),
                saved_identity,
                saved_encoding: Some(encoding),
                saved_line_ending: Some(line_ending),
                language_override: language,
                readonly_reason: readonly,
                paged,
            },
        }
    }
    fn adopt_disk(
        &mut self,
        target: ViewTarget,
        staged: StagedDisk,
        view: ViewState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<TabSlot, ServiceError> {
        let key = target.document;
        match staged.outcome {
            OpenOutcome::Editable { rope, .. } => match plan_hydration_admission(
                self.budget_config,
                &self.collect_hydrated_slots(cx),
                rope.len() as u64,
            ) {
                AdmissionDecision::Admit {
                    dehydrate_documents,
                } => {
                    self.dehydrate_clean_documents(&dehydrate_documents, cx);
                    Ok(TabSlot::Editable(self.make_adapter(
                        target,
                        SessionDocumentStub {
                            doc_id: key.doc_id,
                            lifecycle_gen: key.lifecycle_gen,
                            durable_rev: Revision(0),
                            meta: staged.meta,
                            has_checkpoint: false,
                        },
                        rope,
                        true,
                        false,
                        view,
                        window,
                        cx,
                    )))
                }
                AdmissionDecision::OverBudget { reason, .. } => {
                    Err(failure(ErrorKind::Busy, reason))
                }
            },
            OpenOutcome::Paged {
                doc,
                source_gen,
                readonly_reason,
                ..
            } => Ok(TabSlot::Paged(PagedTabState::new(
                key.doc_id,
                key.lifecycle_gen,
                staged.meta,
                doc,
                source_gen,
                readonly_reason,
                None,
                view.scroll_byte,
            ))),
        }
    }
    fn complete_disk_open(
        &mut self,
        target: ViewTarget,
        seq: u64,
        path: PathBuf,
        result: Result<OpenOutcome, ServiceError>,
        mru: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy() {
            self.deferred_completions.push(DeferredCompletion::Disk {
                target,
                seq,
                path,
                result,
                mru,
            });
            return;
        }
        self.finish_hydration(target, seq);
        let stub = match self.resolve_slot(target) {
            Ok(TabSlot::Hydrating { stub, req_seq }) if *req_seq == seq => stub.clone(),
            _ => {
                self.hydrate_visible_panes(window, cx);
                return;
            }
        };
        if mru {
            if let Some(other) = self
                .session
                .views
                .values()
                .find(|v| {
                    v.document != target.document
                        && self
                            .documents
                            .get(&v.document.doc_id)
                            .is_some_and(|s| s.meta().path.as_ref() == Some(&path))
                })
                .map(|v| ViewTarget {
                    view_id: v.id,
                    document: v.document,
                })
            {
                let _ = self.session.remove_views(&[target]);
                self.documents.remove(&target.document.doc_id);
                self.persistence.metadata.remove(&target.document.doc_id);
                self.persistence.structural = true;
                self.layout_dirty = true;
                self.activate_view(other, window, cx);
                return;
            }
        }
        let result = result.and_then(|outcome| {
            let staged = Self::stage_meta(&path, outcome, stub.meta.language_override.clone());
            let view = self.session.views[&target.view_id].view.clone();
            self.adopt_disk(target, staged, view, window, cx)
        });
        match result {
            Ok(slot) => {
                let metadata_changed = slot.meta() != &stub.meta;
                self.documents.insert(target.document.doc_id, slot);
                if metadata_changed {
                    self.mark_metadata(target.document.doc_id, PendingDocChangeKind::Update);
                }
                if mru {
                    self.record_recent_file(path, window, cx);
                }
                self.ensure_paged_indexer_running(cx);
                if let Some(TabSlot::Paged(pv)) = self.view_slot(target.view_id) {
                    let offset = pv.current_anchor_byte;
                    self.request_paged_viewport(
                        target,
                        ViewportAnchor::ByteOffset(offset),
                        window,
                        cx,
                    );
                }
            }
            Err(e) if e.kind == ErrorKind::Busy => {
                let next_seq = match self.next_operation() {
                    Ok(seq) => seq,
                    Err(e) => {
                        self.documents.insert(
                            stub.doc_id,
                            TabSlot::Failed {
                                stub,
                                error: e.message.clone(),
                            },
                        );
                        self.notify_error(e, window, cx);
                        return;
                    }
                };
                self.documents.insert(
                    stub.doc_id,
                    TabSlot::Hydrating {
                        stub,
                        req_seq: next_seq,
                    },
                );
                self.spawn_disk_open(target, next_seq, path, mru, true, window, cx);
            }
            Err(e) => {
                self.documents.insert(
                    stub.doc_id,
                    TabSlot::Failed {
                        stub,
                        error: e.message.clone(),
                    },
                );
                self.notify_error(e, window, cx);
            }
        }
        if self.active_view_id() == Some(target.view_id) {
            self.focus_active(window, cx);
            if self.search.visible {
                self.run_search_from_inputs(window, cx);
            }
        }
        self.hydrate_visible_panes(window, cx);
        cx.notify();
    }
    pub(crate) fn request_paged_viewport(
        &mut self,
        target: ViewTarget,
        anchor: ViewportAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.resolve(target).is_err() || self.is_lifecycle_busy() {
            return;
        }
        let Some(TabSlot::Paged(pv)) = self.documents.get_mut(&target.document.doc_id) else {
            return;
        };
        let handle = pv.handle.clone();
        let (seq, req, cancel) = pv.prepare_viewport_request(anchor);
        cx.spawn_in(window, async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    loop {
                        match handle.viewport(req.clone(), &cancel) {
                            Err(e) if e.kind == ErrorKind::Busy && !cancel.is_cancelled() => {
                                std::thread::sleep(Duration::from_millis(15))
                            }
                            other => break other,
                        }
                    }
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                this.complete_paged_viewport(target, seq, result, cx)
            });
        })
        .detach();
    }
    fn complete_paged_viewport(
        &mut self,
        target: ViewTarget,
        seq: u64,
        result: Result<editor_services::ViewportSlice, ServiceError>,
        cx: &mut Context<Self>,
    ) {
        if self.session.resolve(target).is_err() {
            return;
        }
        if self.is_lifecycle_busy() {
            self.deferred_completions
                .push(DeferredCompletion::Viewport {
                    target,
                    seq,
                    result,
                });
            return;
        }
        if let Some(TabSlot::Paged(pv)) = self.documents.get_mut(&target.document.doc_id) {
            if pv.apply_viewport_result(seq, result) && pv.source_error.is_some() {
                self.search.clear_results();
            }
        }
        cx.notify();
    }
    pub(super) fn start_background_bootstrap(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shell_path = self.cli.data_dir.join("ui_shell.json");
        cx.spawn_in(window, async move |weak, cx| {
            let boot = cx
                .background_executor()
                .spawn(async move { UiShellStore::open(&shell_path) })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.apply_loaded_shell_bootstrap(boot, window, cx)
            });
        })
        .detach();
        let db = self.cli.data_dir.join("recovery.sqlite3");
        let faults = self.faults.clone();
        let files = self.cli.files.clone();
        cx.spawn_in(window,async move |weak,cx| {
            let (boot,inspection)=cx.background_executor().spawn(async move {
                let boot=(||{
                    let handle=RecoveryHandle::start_with_faults(&db,faults).map_err(|e|{let lost=e.message.starts_with("startup outcome indeterminate:");(e,lost)})?;
                    let manifest_reply=handle.request_manifest().map_err(|e|{let lost=e.kind==ErrorKind::Storage;(e,lost)})?;
                    let manifest=manifest_reply.recv().map_err(|_|(disconnected(),true))?.map_err(|e|(e,false))?;
                    let closed_reply=handle.request_recently_closed(None,64).map_err(|e|{let lost=e.kind==ErrorKind::Storage;(e,lost)})?;
                    let closed=closed_reply.recv().map_err(|_|(disconnected(),true))?.map_err(|e|(e,false))?;
                    Ok::<_,(ServiceError,bool)>((handle,manifest,closed))
                })();
                let inspection=if matches!(&boot,Err((_,true))){Some(editor_services::inspect_recovery_startup(&db))}else{None};
                (boot.map_err(|(error,_)|error),inspection)
            }).await;
            let _=weak.update_in(cx,|this,window,cx| {
                this.startup_metrics.recovery_manifest_ms=Some(this.process_start.elapsed().as_secs_f64()*1000.0);
                match boot {
                    Ok((handle,manifest,closed))=>{this.recovery=Some(handle);this.recovery_ready=true;this.recently_closed=closed.entries;this.apply_recovered_manifest(manifest,files.is_empty(),window,cx);}
                    Err(e)=>{
                        this.recovery_ready=true;this.persistence.failed=true;this.recovery_degraded_error=Some(e.message.clone());
                        match inspection{
                            Some(Ok(editor_services::StartupRecoveryState::AfterImage{manifest,closed}))=>{
                                this.recovery_read_only=true;this.recently_closed=closed.entries;
                                this.apply_recovered_manifest(manifest,files.is_empty(),window,cx);
                                this.recovery_degraded_error=Some("Startup after-image verified read-only; recovery writer is unavailable. Stored drafts remain browsable/exportable. Restart to resume persistence.".into());
                            }
                            Some(Ok(editor_services::StartupRecoveryState::PreImage))=>{
                                this.recovery_degraded_error=Some("Startup pre-image verified; initialization did not commit. Early work retained, recovery storage unavailable.".into());
                            }
                            Some(Err(error))=>{
                                this.bootstrap_indeterminate=true;
                                for slot in this.documents.values(){if let TabSlot::Editable(ed)=slot{ed.editor().update(cx,|state,cx|state.set_readonly(true,cx));}}
                                this.recovery_degraded_error=Some(format!("Startup outcome remains indeterminate: {error}. Early buffers stay frozen; Export Copy is available."));
                            }
                            None=>{}
                        }
                        this.notify_error(failure(ErrorKind::Storage,this.recovery_degraded_error.clone().unwrap_or(e.message)),window,cx);
                    }
                }
                this.open_paths(files,window,cx);
                this.hydrate_visible_panes(window,cx);
                cx.notify();
            });
        }).detach();
    }
    fn apply_recovered_manifest(
        &mut self,
        manifest: SessionManifest,
        hydrate_active: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.persistence.revision = Some(manifest.layout_revision);
        self.recovery_browser.latest_summary = manifest.recovery;
        for warning in manifest.warnings {
            window.push_notification(Notification::warning(warning.message), cx);
        }
        let untouched = self.sole_untouched_scratch(cx).is_some();
        let early = if untouched
            && (!manifest.layout.views.is_empty() || manifest.layout.panes().count() > 1)
        {
            self.documents.clear();
            self.persistence.metadata.clear();
            SessionLayout::default()
        } else {
            std::mem::take(&mut self.session)
        };
        let early_active = early.active_view_id();
        let mut layout = manifest.layout;
        let mut deferred = 0u64;
        let capacity = MAX_SESSION_VIEWS.saturating_sub(early.views.len());
        if layout.views.len() > capacity {
            let targets = layout
                .view_ids()
                .skip(capacity)
                .filter_map(|id| layout.target(id))
                .collect::<Vec<_>>();
            deferred = targets.len() as u64;
            let _ = layout.remove_views(&targets);
            self.persistence.preserve_source = manifest.source_layout_revision;
        }
        for stub in manifest.documents {
            if layout.views.values().any(|v| v.document == stub.key()) {
                self.documents
                    .insert(stub.doc_id, TabSlot::Dehydrated(stub));
            }
        }
        let recovered_pane = layout.active_pane_id();
        for id in early.active_pane().tabs.iter().rev() {
            let mut view = early.views[id].clone();
            view.group_id = GroupId::UNGROUPED;
            let _ = layout.insert_view(recovered_pane, view, Some(0), false);
        }
        if early_active.is_some() {
            layout.pane_mut(recovered_pane).unwrap().active_view = early_active;
        }
        self.session = layout;
        let needs_write = !early.views.is_empty() || deferred > 0 || manifest.repaired_or_fallback;
        self.persistence.structural = needs_write;
        self.layout_dirty = needs_write;
        self.recovery_browser.deferred_on_restore = deferred;
        if hydrate_active {
            self.finish_target_switch(None, window, cx);
        } else {
            self.focus_active(window, cx);
        }
    }
}

impl EditorWorkspace {
    pub(crate) fn save_view(
        &mut self,
        target: ViewTarget,
        explicit_path: Option<PathBuf>,
        force_overwrite: bool,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if close_after_save {
            match self.freeze_lifecycle(vec![target], LifecycleKind::Close, cx) {
                Ok(seq) => {
                    if let Some(p) = self.pending_lifecycle.as_mut() {
                        p.phase = LifecyclePhase::Saving;
                    }
                    self.start_save(
                        target,
                        explicit_path,
                        force_overwrite,
                        Some(seq),
                        window,
                        cx,
                    );
                }
                Err(e) => self.notify_error(e, window, cx),
            }
            return;
        }
        self.start_save(target, explicit_path, force_overwrite, None, window, cx);
    }
    fn start_save(
        &mut self,
        target: ViewTarget,
        explicit_path: Option<PathBuf>,
        force_overwrite: bool,
        owner: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let allowed = match (&self.pending_lifecycle, owner) {
            (None, None) => !self.is_lifecycle_busy(),
            (Some(p), Some(seq)) => p.sequence == seq && matches!(p.phase, LifecyclePhase::Saving),
            _ => false,
        };
        if !allowed || self.pending_saves.contains_key(&target) {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "A save or lifecycle operation is already pending",
                ),
                window,
                cx,
            );
            if let Some(seq) = owner {
                self.cancel_lifecycle(seq, window, cx);
            }
            return;
        }
        let ed = match self.resolve_slot(target) {
            Ok(TabSlot::Editable(ed)) => ed,
            Ok(_) => {
                self.notify_error(
                    failure(
                        ErrorKind::InvalidInput,
                        "Use Export Copy for an unhydrated or paged document",
                    ),
                    window,
                    cx,
                );
                if let Some(seq) = owner {
                    self.cancel_lifecycle(seq, window, cx);
                }
                return;
            }
            Err(e) => {
                self.notify_error(e, window, cx);
                if let Some(seq) = owner {
                    self.cancel_lifecycle(seq, window, cx);
                }
                return;
            }
        };
        if ed.has_pending_native_edit(cx) {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "A native edit is awaiting delivery; retry Save",
                ),
                window,
                cx,
            );
            if let Some(seq) = owner {
                self.cancel_lifecycle(seq, window, cx);
            }
            return;
        }
        let Some(path) = explicit_path.or_else(|| ed.meta.path.clone()) else {
            self.prompt_save_owned(target, owner, window, cx);
            return;
        };
        let text_seq = ed.current_text_seq();
        let request = SaveRequest {
            doc_id: ed.doc_id,
            lifecycle_gen: ed.lifecycle_gen,
            captured_rev: ed.visible_revision,
            rope: ed.committed_rope().clone(),
            target_path: path.clone(),
            encoding: ed.meta.encoding,
            line_ending: ed.meta.line_ending,
            expected_fingerprint: if ed.meta.path.as_ref() == Some(&path) {
                ed.meta.origin_fingerprint.clone()
            } else {
                None
            },
            force_overwrite,
        };
        let seq = match self.next_operation() {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(e, window, cx);
                if let Some(owner) = owner {
                    self.cancel_lifecycle(owner, window, cx);
                }
                return;
            }
        };
        self.pending_saves.insert(target, seq);
        if let Some(owner) = owner {
            if let Some(p) = self
                .pending_lifecycle
                .as_mut()
                .filter(|p| p.sequence == owner)
            {
                if let Some(f) = p.frozen.iter_mut().find(|f| f.target == target) {
                    f.saved_in_batch = true;
                }
            }
        }
        let recovery = self.recovery.clone();
        let faults = self.faults.clone();
        let encoding = request.encoding;
        let eol = request.line_ending;
        cx.spawn_in(window,async move |weak,cx| {
            let (outcome,path)=cx.background_executor().spawn(async move {let outcome=save_document(&request,recovery.as_ref(),&faults);let path=std::fs::canonicalize(&path).unwrap_or(path);(outcome,path)}).await;
            let _=weak.update_in(cx,|this,window,cx| {
                if this.pending_saves.get(&target)!=Some(&seq){return;}
                let exact=this.session.resolve(target).is_ok();
                let saved=matches!(outcome,SaveOutcome::Saved {..});
                let mut identity=None;
                if exact {
                    match outcome {
                        SaveOutcome::Saved {captured_rev,captured_identity,fingerprint,..}|SaveOutcome::SavedWithRecoveryWarning {captured_rev,captured_identity,fingerprint,..}=>{
                            let mut check=None;
                            if let Some(TabSlot::Editable(ed))=this.documents.get_mut(&target.document.doc_id){
                                check=ed.mark_saved(path.clone(),captured_rev,text_seq,captured_identity,fingerprint,encoding,eol,window,cx).map(|c|(c,ed.committed_rope().clone()));
                                if ed.visible_revision==captured_rev&&ed.current_text_seq()==text_seq{identity=Some(captured_identity);}
                            }
                            if let Some((c,rope))=check{this.spawn_savepoint_verification(target,c.seq,c.checked_text_seq,c.saved_identity,rope,cx);}
                            this.mark_metadata(target.document.doc_id,PendingDocChangeKind::Update);this.record_recent_file(path.clone(),window,cx);
                            this.status_flash=Some((format!("Saved {}",path.display()),Instant::now()));
                            if !saved{this.notify_error(failure(ErrorKind::Storage,"File saved, but recovery metadata was not acknowledged; close cancelled"),window,cx);}
                        }
                        SaveOutcome::ConflictDetected {..}=>{
                            this.notify_error(failure(ErrorKind::Conflict,"File changed on disk; draft retained"),window,cx);
                            if owner.is_none(){this.open_save_conflict(target,path,window,cx);}
                        }
                        SaveOutcome::Failed {error,..}=>this.notify_error(error,window,cx),
                        SaveOutcome::PublishedNotDurable {warning,..}=>this.notify_error(failure(ErrorKind::PublishedNotDurable,format!("File published but durability unconfirmed; draft stays modified: {}",warning.message)),window,cx),
                    }
                }
                // This UI completion owns the pending-save gate through association application.
                this.pending_saves.remove(&target);
                if let Some(owner)=owner {
                    if saved&&exact {
                        if let Some(p)=this.pending_lifecycle.as_mut().filter(|p|p.sequence==owner){if let Some(f)=p.frozen.iter_mut().find(|f|f.target==target){f.identity=identity;}}
                        this.continue_lifecycle_saves(owner,window,cx);
                    } else {this.cancel_lifecycle(owner,window,cx);}
                }
                cx.notify();
            });
        }).detach();
    }
    pub(crate) fn prompt_save_view_as(
        &mut self,
        target: ViewTarget,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if close_after_save {
            match self.freeze_lifecycle(vec![target], LifecycleKind::Close, cx) {
                Ok(seq) => {
                    if let Some(p) = self.pending_lifecycle.as_mut() {
                        p.phase = LifecyclePhase::Saving;
                    }
                    self.prompt_save_owned(target, Some(seq), window, cx)
                }
                Err(e) => self.notify_error(e, window, cx),
            }
        } else {
            self.prompt_save_owned(target, None, window, cx);
        }
    }
    fn prompt_save_owned(
        &mut self,
        target: ViewTarget,
        owner: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let slot = match self.resolve_slot(target) {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(e, window, cx);
                if let Some(seq) = owner {
                    self.cancel_lifecycle(seq, window, cx);
                }
                return;
            }
        };
        if owner.is_none() && self.is_lifecycle_busy() {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "Lifecycle operation pending; use Export Copy for rescue",
                ),
                window,
                cx,
            );
            return;
        }
        let dir = slot
            .meta()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let name = slot
            .meta()
            .path
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|s| s.to_str())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}.txt", slot.title()));
        let reply = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |weak, cx| {
            let chosen = reply.await;
            let _ = weak.update_in(cx, |this, window, cx| match chosen {
                Ok(Ok(Some(path))) => this.start_save(target, Some(path), true, owner, window, cx),
                _ => {
                    if let Some(seq) = owner {
                        this.cancel_lifecycle(seq, window, cx);
                    }
                }
            });
        })
        .detach();
    }
    fn open_save_conflict(
        &self,
        target: ViewTarget,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        window.open_dialog(cx,move |dialog,_,_|{
            let entity=entity.clone();let path=path.clone();
            dialog.title("External File Change Detected").button_props(DialogButtonProps::default().ok_text("Overwrite Disk File").ok_variant(ButtonVariant::Danger).show_cancel(true).cancel_text("Keep Draft & Cancel").on_ok(move |_,window,cx|{entity.update(cx,|this,cx|this.save_view(target,Some(path.clone()),true,false,window,cx));true}))
                .footer(h_flex().gap_2().justify_end().child(gpui_kit::component::dialog::DialogClose::new().trigger(|button|button.label("Keep Draft & Cancel"))).child(gpui_kit::component::dialog::DialogAction::new().child(Button::new("overwrite-disk-file").danger().label("Overwrite Disk File"))))
                .child("Overwrite the changed disk file with this document, or cancel and choose Save As?")
        });
    }
    fn freeze_lifecycle(
        &mut self,
        targets: Vec<ViewTarget>,
        kind: LifecycleKind,
        cx: &mut Context<Self>,
    ) -> Result<u64, ServiceError> {
        if self.is_lifecycle_busy() {
            return Err(failure(
                ErrorKind::Busy,
                "Another lifecycle operation is pending",
            ));
        }
        if self.recovery.is_none() || self.persistence.revision.is_none() || self.persistence.failed
        {
            return Err(failure(
                ErrorKind::Storage,
                "Available acknowledged recovery storage is required",
            ));
        }
        if targets.is_empty() {
            return Err(failure(ErrorKind::InvalidInput, "No views to close"));
        }
        for target in &targets {
            self.resolve_slot(*target)?;
            if self.pending_saves.contains_key(target) {
                return Err(failure(
                    ErrorKind::Busy,
                    "A save has not finished its UI completion",
                ));
            }
        }
        // Check every editor BEFORE any readonly mutation. Queued native edits are not post-freeze edits.
        if self
            .documents
            .values()
            .any(|s| matches!(s,TabSlot::Editable(ed) if ed.has_pending_native_edit(cx)))
        {
            return Err(failure(
                ErrorKind::Busy,
                "A committed native edit is queued; retry after it is delivered",
            ));
        }
        let sequence = self.next_operation()?;
        self.pane_runtime.resize = None;
        let mut frozen = Vec::with_capacity(targets.len());
        for target in targets {
            self.capture_view(target.view_id, cx);
            let slot = self.resolve_slot(target)?;
            let (revision, text_seq, oversized) = match slot {
                TabSlot::Editable(ed) => (
                    Some(ed.visible_revision),
                    Some(ed.current_text_seq()),
                    ed.committed_byte_len() > MAX_ORDINARY_DOC_BYTES as u64,
                ),
                TabSlot::Dehydrated(s)
                | TabSlot::Hydrating { stub: s, .. }
                | TabSlot::OverBudgetDraft { stub: s, .. }
                | TabSlot::Failed { stub: s, .. } => {
                    (s.has_checkpoint.then_some(s.durable_rev), None, false)
                }
                TabSlot::Paged(_) => (None, None, false),
            };
            frozen.push(FrozenView {
                target,
                revision,
                text_seq,
                protected: slot.close_guard(),
                oversized,
                identity: None,
                format: (slot.meta().encoding, slot.meta().line_ending),
                view: self.session.views[&target.view_id].view.clone(),
                disposition: CloseDisposition::KeepRecovery,
                saved_in_batch: false,
            });
        }
        let mut original_readonly = Vec::new();
        for slot in self.documents.values_mut() {
            if let TabSlot::Editable(ed) = slot {
                let original = ed.editor().read(cx).presentation().is_readonly();
                original_readonly.push((ed.doc_id, original));
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
            }
        }
        self.search.cancel_active();
        self.pending_lifecycle = Some(PendingLifecycle {
            sequence,
            started: Instant::now(),
            kind,
            phase: LifecyclePhase::Confirming,
            frozen,
            original_readonly,
            dialog_dropped: Arc::new(AtomicBool::new(false)),
            cancel: CancelToken::new(),
            disconnected: false,
            reconciliation_started: false,
            reconciliation: None,
            accepted_revision: None,
            admitted_sequence: None,
        });
        cx.notify();
        Ok(sequence)
    }
    pub(crate) fn request_close_view(
        &mut self,
        target: ViewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_close_targets(vec![target], window, cx);
    }
    pub(crate) fn request_close_group(
        &mut self,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session.group(group).is_none() {
            self.notify_error(
                failure(ErrorKind::Stale, "Group no longer exists"),
                window,
                cx,
            );
            return;
        }
        let targets = self
            .all_view_ids()
            .filter_map(|id| self.session.view(id))
            .filter(|v| v.group_id == group)
            .map(|v| ViewTarget {
                view_id: v.id,
                document: v.document,
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return;
        }
        self.request_close_targets(targets, window, cx);
    }
    fn request_close_targets(
        &mut self,
        targets: Vec<ViewTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let seq = match self.freeze_lifecycle(targets, LifecycleKind::Close, cx) {
            Ok(seq) => seq,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        if self
            .pending_lifecycle
            .as_ref()
            .is_some_and(|p| p.frozen.iter().any(|f| f.protected))
        {
            self.open_close_confirmation(seq, window, cx);
        } else {
            self.prepare_lifecycle(seq, window, cx);
        }
    }
    fn open_close_confirmation(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self
            .pending_lifecycle
            .as_ref()
            .filter(|p| p.sequence == seq)
        else {
            return;
        };
        let total = p.frozen.len();
        let protected = p.frozen.iter().filter(|f| f.protected).count();
        let unknown = p
            .frozen
            .iter()
            .filter(|f| f.protected && f.text_seq.is_none())
            .count();
        let can_keep = p.frozen.iter().all(|f| !f.oversized || !f.protected);
        let can_save = p
            .frozen
            .iter()
            .filter(|f| f.protected)
            .all(|f| matches!(self.view_slot(f.target.view_id), Some(TabSlot::Editable(_))));
        let export_target = p.frozen.iter().find(|f| f.protected).map(|f| f.target);
        let guard = Rc::new(DialogLifetime(p.dialog_dropped.clone()));
        let entity = cx.entity();
        window.open_dialog(cx,move |dialog,_,_|{
            let _guard=guard.clone();
            let keep=entity.clone();let discard=entity.clone();let save=entity.clone();let cancel=entity.clone();let export=entity.clone();let dismiss=entity.clone();let confirm_keep=entity.clone();
            let mut footer=h_flex().gap_2().justify_end()
                .child(Button::new("lifecycle-cancel").label("Cancel").on_click(move |_,w,c|{cancel.update(c,|this,c|this.cancel_lifecycle(seq,w,c));w.close_dialog(c);}))
                .child(Button::new("lifecycle-discard").danger().label("Discard Protected Drafts & Close").on_click(move |_,w,c|{discard.update(c,|this,c|this.choose_discard(seq,w,c));w.close_dialog(c);}));
            if can_keep{footer=footer.child(Button::new("lifecycle-keep").label("Keep Stored Drafts & Close").on_click(move |_,w,c|{keep.update(c,|this,c|this.prepare_lifecycle(seq,w,c));w.close_dialog(c);}));}
            if can_save{footer=footer.child(Button::new("lifecycle-save").primary().label("Save & Close").on_click(move |_,w,c|{save.update(c,|this,c|this.begin_lifecycle_saves(seq,w,c));w.close_dialog(c);}));}
            if let Some(target)=export_target{footer=footer.child(Button::new("lifecycle-export").label("Cancel & Export Copy…").on_click(move |_,w,c|{export.update(c,|this,c|{this.cancel_lifecycle(seq,w,c);this.export_view_draft_copy(target,w,c);});w.close_dialog(c);}));}
            dialog.title(format!("Close {total} document{}?",if total==1{""}else{"s"}))
                .button_props(DialogButtonProps::default().on_ok(move |_,w,c|{if !can_keep{return false;}confirm_keep.update(c,|this,c|this.prepare_lifecycle(seq,w,c));true}))
                .on_close(move |_,w,c|{dismiss.update(c,|this,c|{if this.pending_lifecycle.as_ref().is_some_and(|p|p.sequence==seq&&matches!(p.phase,LifecyclePhase::Confirming)){this.cancel_lifecycle(seq,w,c);}});}).footer(footer).child(format!("{protected} protected drafts; {unknown} have not been compared with disk. Keep preserves stored drafts without loading them. Discard permanently deletes only protected members; clean members remain reopenable. Oversized unsaved buffers must be saved or explicitly discarded."))
        });
    }
    pub(crate) fn choose_discard(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let p = self
                .pending_lifecycle
                .as_mut()
                .filter(|p| {
                    p.sequence == seq
                        && matches!(p.phase, LifecyclePhase::Confirming)
                        && matches!(p.kind, LifecycleKind::Close)
                })
                .ok_or_else(|| failure(ErrorKind::Stale, "Close operation changed"))?;
            for f in &mut p.frozen {
                if f.protected {
                    if f.saved_in_batch {
                        return Err(failure(
                            ErrorKind::Busy,
                            "Cannot discard a member with an in-batch save",
                        ));
                    }
                    f.disposition = CloseDisposition::Discard {
                        tombstone_gen: LifecycleGen(
                            f.target
                                .document
                                .lifecycle_gen
                                .0
                                .checked_add(1)
                                .ok_or_else(|| {
                                    failure(ErrorKind::Storage, "Lifecycle exhausted")
                                })?,
                        ),
                    };
                }
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.notify_error(e, window, cx);
            self.cancel_lifecycle(seq, window, cx);
        } else {
            self.prepare_lifecycle(seq, window, cx);
        }
    }
    fn begin_lifecycle_saves(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pending_lifecycle.as_mut().filter(|p| {
            p.sequence == seq
                && matches!(p.phase, LifecyclePhase::Confirming)
                && matches!(p.kind, LifecycleKind::Close)
        }) else {
            return;
        };
        p.phase = LifecyclePhase::Saving;
        self.continue_lifecycle_saves(seq, window, cx);
    }
    fn continue_lifecycle_saves(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self
            .pending_lifecycle
            .as_ref()
            .filter(|p| p.sequence == seq && matches!(p.phase, LifecyclePhase::Saving))
        else {
            return;
        };
        let next = p
            .frozen
            .iter()
            .find(|f| f.protected && !f.saved_in_batch)
            .map(|f| f.target);
        if let Some(target) = next {
            self.start_save(target, None, false, Some(seq), window, cx);
        } else {
            self.prepare_lifecycle(seq, window, cx);
        }
    }
    fn prepare_lifecycle(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.pending_lifecycle.as_mut().filter(|p| {
            p.sequence == seq
                && matches!(p.phase, LifecyclePhase::Confirming | LifecyclePhase::Saving)
        }) else {
            return;
        };
        p.phase = LifecyclePhase::Preparing;
        let identities = p
            .frozen
            .iter()
            .filter(|f| {
                f.oversized
                    && f.identity.is_none()
                    && matches!(f.disposition, CloseDisposition::KeepRecovery)
            })
            .filter_map(|f| match self.documents.get(&f.target.document.doc_id) {
                Some(TabSlot::Editable(ed)) => Some((f.target, ed.committed_rope().clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        if identities.is_empty() {
            self.finish_lifecycle_preparation(seq, Vec::new(), window, cx);
            return;
        }
        cx.spawn_in(window, async move |weak, cx| {
            let identities = cx
                .background_executor()
                .spawn(async move {
                    identities
                        .into_iter()
                        .map(|(target, rope)| (target, ContentIdentity::of_rope(&rope)))
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = weak.update_in(cx, |this, w, c| {
                this.finish_lifecycle_preparation(seq, identities, w, c)
            });
        })
        .detach();
    }
    fn finish_lifecycle_preparation(
        &mut self,
        seq: u64,
        identities: Vec<(ViewTarget, ContentIdentity)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self
            .pending_lifecycle
            .as_mut()
            .filter(|p| p.sequence == seq && matches!(p.phase, LifecyclePhase::Preparing))
        else {
            return;
        };
        for (target, identity) in identities {
            if let Some(f) = p.frozen.iter_mut().find(|f| f.target == target) {
                f.identity = Some(identity);
            }
        }
        p.phase = LifecyclePhase::Ready;
        self.pump_session(window, cx);
    }
    pub(crate) fn request_reload_view(
        &mut self,
        target: ViewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = match self.resolve_slot(target) {
            Ok(s) => s.meta().path.clone(),
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        let Some(path) = path else {
            self.notify_error(
                failure(
                    ErrorKind::InvalidInput,
                    "Untitled documents cannot reload from disk",
                ),
                window,
                cx,
            );
            return;
        };
        let fresh = DocumentKey {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
        };
        let seq = match self.freeze_lifecycle(
            vec![target],
            LifecycleKind::Reload {
                path,
                fresh,
                staged: None,
                record_mru: false,
            },
            cx,
        ) {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        if !self.pending_lifecycle.as_ref().unwrap().frozen[0].protected {
            self.stage_reload(seq, window, cx);
            return;
        }
        let entity = cx.entity();
        let guard = Rc::new(DialogLifetime(
            self.pending_lifecycle
                .as_ref()
                .unwrap()
                .dialog_dropped
                .clone(),
        ));
        window.open_dialog(cx,move |dialog,_,_|{
            let _guard=guard.clone();let accept=entity.clone();let cancel=entity.clone();
            dialog.title("Discard Local Edits / Stored Recovery Draft & Reload from Disk?")
                .button_props(DialogButtonProps::default().ok_text("Discard & Reload from Disk").ok_variant(ButtonVariant::Danger).show_cancel(true).cancel_text("Cancel").on_ok(move |_,w,c|{accept.update(c,|this,c|this.stage_reload(seq,w,c));true}))
                .footer(h_flex().gap_2().justify_end().child(gpui_kit::component::dialog::DialogClose::new().trigger(|button|button.label("Cancel"))).child(gpui_kit::component::dialog::DialogAction::new().child(Button::new("reload-confirm").danger().label("Discard & Reload from Disk"))))
                .on_close(move |_,w,c|{cancel.update(c,|this,c|{if this.pending_lifecycle.as_ref().is_some_and(|p|p.sequence==seq&&matches!(p.phase,LifecyclePhase::Confirming)){this.cancel_lifecycle(seq,w,c);}});})
                .child("This destroys live edits that were never checkpointed, including oversized buffers, and all stored recovery content for this document. They cannot be reopened. Cancel and Save As / Export Copy first if you want to retain those bytes. The current disk file is verified before any stored draft is discarded.")
        });
    }
    fn stage_reload(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self
            .pending_lifecycle
            .as_mut()
            .filter(|p| p.sequence == seq && matches!(p.phase, LifecyclePhase::Confirming))
        else {
            return;
        };
        let LifecycleKind::Reload { path, fresh, .. } = &p.kind else {
            return;
        };
        let path = path.clone();
        let fresh = *fresh;
        let target = p.frozen[0].target;
        let cancel = p.cancel.clone();
        p.phase = LifecyclePhase::Preparing;
        let language = self.documents[&target.document.doc_id]
            .meta()
            .language_override
            .clone();
        let (options, force_paged) = self.disk_options(Some(target.document.doc_id), cx);
        let paging = self.paging_service.clone();
        cx.spawn_in(window, async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let path = std::fs::canonicalize(&path)?;
                    let outcome = open_disk_for_budget(
                        fresh.doc_id,
                        &path,
                        &paging,
                        options,
                        force_paged,
                        &cancel,
                    )?;
                    if cancel.is_cancelled() {
                        return Err(failure(ErrorKind::Cancelled, "Reload cancelled"));
                    }
                    Ok::<_, ServiceError>(Self::stage_meta(&path, outcome, language))
                })
                .await;
            let _ = weak.update_in(cx, |this, w, c| {
                let Some(p) = this
                    .pending_lifecycle
                    .as_mut()
                    .filter(|p| p.sequence == seq)
                else {
                    return;
                };
                match result {
                    Ok(result) => {
                        if let LifecycleKind::Reload { staged, .. } = &mut p.kind {
                            *staged = Some(result);
                        }
                        p.phase = LifecyclePhase::Ready;
                        this.pump_session(w, c);
                    }
                    Err(e) => {
                        this.notify_error(e, w, c);
                        this.cancel_lifecycle(seq, w, c);
                    }
                }
            });
        })
        .detach();
    }
    pub(super) fn cancel_pending_lifecycle(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(seq) = self.pending_lifecycle.as_ref().map(|p| p.sequence) else {
            return false;
        };
        self.cancel_open_batch(window, cx);
        self.cancel_lifecycle(seq, window, cx);
        true
    }
    fn cancel_lifecycle(&mut self, seq: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .persistence
            .inflight
            .as_ref()
            .is_some_and(|flight| flight.disconnected)
        {
            return;
        }
        if self.pending_lifecycle.as_ref().is_none_or(|p| {
            p.sequence != seq
                || matches!(
                    p.phase,
                    LifecyclePhase::Closing { .. } | LifecyclePhase::Replacing { .. }
                )
        }) {
            return;
        }
        let p = self.pending_lifecycle.take().unwrap();
        p.cancel.cancel();
        self.restore_readonly(p.original_readonly, cx);
        self.finish_lifecycle(window, cx);
    }
    pub(super) fn restore_readonly(
        &self,
        original: Vec<(DocumentId, bool)>,
        cx: &mut Context<Self>,
    ) {
        for (id, readonly) in original {
            if let Some(TabSlot::Editable(ed)) = self.documents.get(&id) {
                ed.editor().update(cx, |s, cx| s.set_readonly(readonly, cx));
            }
        }
    }
    fn finish_lifecycle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let deferred = std::mem::take(&mut self.deferred_completions);
        for completion in deferred {
            match completion {
                DeferredCompletion::Hydrated {
                    target,
                    seq,
                    result,
                } => self.complete_hydration(target, seq, result, window, cx),
                DeferredCompletion::Disk {
                    target,
                    seq,
                    path,
                    result,
                    mru,
                } => self.complete_disk_open(target, seq, path, result, mru, window, cx),
                DeferredCompletion::Viewport {
                    target,
                    seq,
                    result,
                } => self.complete_paged_viewport(target, seq, result, cx),
            }
        }
        self.drain_open_paths(window, cx);
        self.focus_active(window, cx);
        cx.notify();
        if self.pending_lifecycle.is_none() && self.pending_open_paths.is_empty() {
            if let Some(force) = self.pending_quit.take() {
                self.initiate_clean_quit(force, window, cx);
            }
        }
    }
    pub(super) fn pump_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut flight) = self.persistence.inflight.take() {
            let mut outcome = match flight.reply.try_recv() {
                Ok(Ok(ack)) if ack.sequence == flight.sequence => Some(Ok(ack)),
                Ok(Err(e)) => Some(Err(e)),
                Ok(Ok(_)) | Err(TryRecvError::Disconnected) => {
                    flight.disconnected = true;
                    None
                }
                Err(TryRecvError::Empty) => None,
            };
            if flight.disconnected {
                self.cancel_open_batch(window, cx);
                self.persistence.failed = true;
                self.pane_runtime.resize = None;
                if flight.original_readonly.is_none() {
                    let mut originals = Vec::new();
                    for slot in self.documents.values_mut() {
                        if let TabSlot::Editable(ed) = slot {
                            originals.push((
                                ed.doc_id,
                                ed.editor().read(cx).presentation().is_readonly(),
                            ));
                            ed.editor()
                                .update(cx, |state, cx| state.set_readonly(true, cx));
                        }
                    }
                    flight.original_readonly = Some(originals);
                }
                if let Some(inspection) = flight.inspection.take() {
                    match inspection {
                        Ok((manifest, _)) => {
                            let after = match &flight.layout {
                                LayoutPayload::Structural(layout) => manifest.layout == *layout,
                                LayoutPayload::TreeOnly(tree) => manifest.layout.tree == *tree,
                            };
                            if after && manifest.layout_revision > flight.expected_revision {
                                outcome = Some(Ok(LayoutAck {
                                    sequence: flight.sequence,
                                    revision: manifest.layout_revision,
                                }));
                            } else if manifest.layout_revision == flight.expected_revision {
                                outcome = Some(Err(failure(
                                    ErrorKind::Storage,
                                    "Read-only reconciliation proves session write did not commit; live work retained",
                                )));
                            } else {
                                self.recovery_degraded_error=Some("Session outcome remains indeterminate after read-only inspection; frozen drafts remain exportable".into());
                            }
                        }
                        Err(e) => {
                            self.recovery_degraded_error = Some(format!(
                                "Session reconciliation failed: {e}; frozen drafts remain exportable"
                            ))
                        }
                    }
                } else if !flight.inspection_started {
                    flight.inspection_started = true;
                    let sequence = flight.sequence;
                    let db = self.cli.data_dir.join("recovery.sqlite3");
                    cx.spawn(async move |weak, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move { editor_services::inspect_recovery_session(db) })
                            .await;
                        let _ = weak.update(cx, |this, cx| {
                            if let Some(flight) = this
                                .persistence
                                .inflight
                                .as_mut()
                                .filter(|flight| flight.sequence == sequence)
                            {
                                flight.inspection = Some(result);
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                }
            }
            if let Some(outcome) = outcome {
                if flight.disconnected {
                    self.adopt_read_only_recovery();
                }
                if let Some(originals) = flight.original_readonly.take() {
                    self.restore_readonly(originals, cx);
                }
                match outcome {
                    Ok(ack) => {
                        self.persistence.revision = Some(ack.revision);
                        self.persistence.failed = flight.disconnected;
                        for (id, kind) in flight.intents {
                            if matches!(kind, PendingDocChangeKind::Reopen)
                                && self.persistence.metadata_after_reopen.remove(&id)
                                && self.documents.contains_key(&id)
                            {
                                self.mark_metadata(id, PendingDocChangeKind::Update);
                            }
                        }
                    }
                    Err(e) => {
                        for (id, kind) in flight.intents {
                            if self.documents.contains_key(&id) {
                                self.persistence.metadata.entry(id).or_insert(kind);
                            }
                        }
                        self.persistence.preserve_source =
                            self.persistence.preserve_source.or(flight.preserve_source);
                        self.persistence.structural = true;
                        self.layout_dirty = true;
                        self.persistence.failed = true;
                        self.notify_error(e, window, cx);
                        if let Some(seq) = self.pending_lifecycle.as_ref().map(|p| p.sequence) {
                            self.cancel_lifecycle(seq, window, cx);
                        }
                    }
                }
                if flight.disconnected && self.pending_lifecycle.is_none() {
                    self.finish_lifecycle(window, cx);
                }
            } else {
                if flight.started.elapsed() > Duration::from_secs(15) {
                    self.recovery_degraded_error=Some("Session write is pending/indeterminate; its receiver and live documents are retained".into());
                }
                self.persistence.inflight = Some(flight);
            }
        }
        if self.persistence.revision.is_some()
            && !self.persistence.failed
            && self.pending_lifecycle.as_ref().is_none_or(|p| {
                matches!(
                    p.phase,
                    LifecyclePhase::Confirming
                        | LifecyclePhase::Preparing
                        | LifecyclePhase::Saving
                        | LifecyclePhase::Ready
                )
            })
        {
            if let Err(e) = self.admit_layout() {
                if e.kind != ErrorKind::Busy && e.kind != ErrorKind::Stale {
                    self.persistence.failed = true;
                    self.notify_error(e, window, cx);
                }
            }
        }
        if self.persistence.failed && self.persistence.inflight.is_none() {
            let unadmitted = self
                .pending_lifecycle
                .as_ref()
                .filter(|pending| {
                    !pending.disconnected
                        && !matches!(
                            pending.phase,
                            LifecyclePhase::Closing { .. } | LifecyclePhase::Replacing { .. }
                        )
                })
                .map(|pending| pending.sequence);
            if let Some(sequence) = unadmitted {
                self.notify_error(failure(ErrorKind::Storage,"Lifecycle operation cancelled before admission because recovery storage is unavailable; live views and drafts were retained"),window,cx);
                self.cancel_lifecycle(sequence, window, cx);
            }
        }
        let Some(mut p) = self.pending_lifecycle.take() else {
            if self.pending_quit.is_some()
                && self.persistence.inflight.is_none()
                && (!self.layout_dirty || self.persistence.failed)
                && !self.is_lifecycle_busy()
            {
                let force = self.pending_quit.take().unwrap();
                self.initiate_clean_quit(force, window, cx);
            }
            return;
        };
        if matches!(p.phase, LifecyclePhase::Confirming) && p.dialog_dropped.load(Ordering::Acquire)
        {
            let seq = p.sequence;
            self.pending_lifecycle = Some(p);
            self.cancel_lifecycle(seq, window, cx);
            return;
        }
        if matches!(p.phase, LifecyclePhase::Ready)
            && self.persistence.inflight.is_none()
            && !self.layout_dirty
            && !self.persistence.failed
        {
            let admission = self.admit_lifecycle(&mut p, cx);
            if let Err(e) = admission {
                if e.kind != ErrorKind::Busy {
                    let seq = p.sequence;
                    self.pending_lifecycle = Some(p);
                    self.notify_error(e, window, cx);
                    self.cancel_lifecycle(seq, window, cx);
                    return;
                }
            }
        }
        let mut outcome = match &p.phase {
            LifecyclePhase::Closing { reply, .. } => match reply.try_recv() {
                Ok(result) => Some(result.map(|ack| (ack.layout, Some(ack.closed)))),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    p.disconnected = true;
                    self.recovery_degraded_error = Some(disconnected().message);
                    None
                }
            },
            LifecyclePhase::Replacing { reply, .. } => match reply.try_recv() {
                Ok(result) => Some(result.map(|ack| (ack, None))),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    p.disconnected = true;
                    self.recovery_degraded_error = Some(disconnected().message);
                    None
                }
            },
            _ => None,
        };
        if p.disconnected {
            self.cancel_open_batch(window, cx);
            self.persistence.failed = true;
            if let Some(result) = p.reconciliation.take() {
                match result {
                    Ok((manifest, closed)) => {
                        let after = match &p.phase {
                            LifecyclePhase::Closing { after, .. }
                            | LifecyclePhase::Replacing { after, .. } => Some(after),
                            _ => None,
                        };
                        if after == Some(&manifest.layout)
                            && Some(manifest.layout_revision) > p.accepted_revision
                        {
                            outcome = Some(Ok((
                                LayoutAck {
                                    sequence: p.admitted_sequence.unwrap(),
                                    revision: manifest.layout_revision,
                                },
                                matches!(p.kind, LifecycleKind::Close).then_some(closed.entries),
                            )));
                        } else if Some(manifest.layout_revision) == p.accepted_revision
                            && manifest.layout == self.session
                        {
                            outcome = Some(Err(failure(
                                ErrorKind::Storage,
                                "Read-only reconciliation proves the lifecycle transaction did not commit; buffers retained",
                            )));
                        } else {
                            self.recovery_degraded_error=Some("Read-only reconciliation could not establish the lifecycle outcome; frozen buffers retained for export".into());
                        }
                    }
                    Err(e) => {
                        self.recovery_degraded_error = Some(format!(
                            "Read-only reconciliation failed: {e}; frozen buffers retained for export"
                        ))
                    }
                }
            } else if !p.reconciliation_started {
                p.reconciliation_started = true;
                let sequence = p.sequence;
                let db = self.cli.data_dir.join("recovery.sqlite3");
                cx.spawn(async move |weak, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move { editor_services::inspect_recovery_session(db) })
                        .await;
                    let _ = weak.update(cx, |this, cx| {
                        if let Some(p) = this
                            .pending_lifecycle
                            .as_mut()
                            .filter(|p| p.sequence == sequence)
                        {
                            p.reconciliation = Some(result);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
        if let Some(result) = outcome {
            if p.disconnected {
                self.adopt_read_only_recovery();
            }
            match result {
                Ok((ack, closed)) => {
                    let outgoing = self.active_view_id().and_then(|id| self.view_target(id));
                    let after = match &p.phase {
                        LifecyclePhase::Closing { after, .. }
                        | LifecyclePhase::Replacing { after, .. } => after.clone(),
                        _ => unreachable!(),
                    };
                    for f in &p.frozen {
                        self.cancel_hydration(f.target);
                    }
                    let mut replacement = None;
                    if let Some(closed) = closed {
                        for f in &p.frozen {
                            self.documents.remove(&f.target.document.doc_id);
                        }
                        if let LifecyclePhase::Closing {
                            scratch: Some(stub),
                            ..
                        } = &p.phase
                        {
                            self.documents
                                .insert(stub.doc_id, TabSlot::Dehydrated(stub.clone()));
                        }
                        for record in closed {
                            self.recently_closed
                                .retain(|c| c.document.doc_id != record.document.doc_id);
                            self.recently_closed.insert(0, record);
                        }
                        self.recently_closed.truncate(64);
                    } else if let LifecycleKind::Reload {
                        fresh,
                        staged: Some(staged),
                        record_mru,
                        ..
                    } = p.kind
                    {
                        let old = p.frozen[0].target.document.doc_id;
                        self.documents.remove(&old);
                        self.recently_closed.retain(|c| c.document.doc_id != old);
                        let view = after.views[&p.frozen[0].target.view_id].view.clone();
                        let meta = staged.meta.clone();
                        let slot = self
                            .adopt_disk(
                                ViewTarget {
                                    view_id: p.frozen[0].target.view_id,
                                    document: fresh,
                                },
                                staged,
                                view,
                                window,
                                cx,
                            )
                            .unwrap_or_else(|e| TabSlot::Failed {
                                stub: SessionDocumentStub {
                                    doc_id: fresh.doc_id,
                                    lifecycle_gen: fresh.lifecycle_gen,
                                    durable_rev: Revision(0),
                                    meta,
                                    has_checkpoint: false,
                                },
                                error: e.message,
                            });
                        self.documents.insert(fresh.doc_id, slot);
                        replacement = Some((
                            ViewTarget {
                                view_id: p.frozen[0].target.view_id,
                                document: fresh,
                            },
                            record_mru,
                        ));
                    }
                    self.session = after;
                    self.persistence.revision = Some(ack.revision);
                    self.persistence
                        .metadata
                        .retain(|id, _| self.documents.contains_key(id));
                    self.persistence.structural = false;
                    self.layout_dirty = !self.persistence.metadata.is_empty();
                    self.restore_readonly(p.original_readonly, cx);
                    if let Some((target, record_mru)) = replacement {
                        if record_mru
                            && let Some(slot) = self.view_slot(target.view_id)
                            && matches!(slot, TabSlot::Editable(_) | TabSlot::Paged(_))
                            && let Some(path) = slot.meta().path.clone()
                        {
                            self.record_recent_file(path, window, cx);
                        }
                        self.ensure_paged_indexer_running(cx);
                        if let Some(TabSlot::Paged(paged)) = self.view_slot(target.view_id) {
                            let offset = paged.current_anchor_byte;
                            self.request_paged_viewport(
                                target,
                                ViewportAnchor::ByteOffset(offset),
                                window,
                                cx,
                            );
                        }
                    }
                    self.finish_target_switch(outgoing, window, cx);
                    self.finish_lifecycle(window, cx);
                    return;
                }
                Err(e) => {
                    self.restore_readonly(p.original_readonly, cx);
                    self.notify_error(e, window, cx);
                    self.finish_lifecycle(window, cx);
                    return;
                }
            }
        }
        if p.started.elapsed() > Duration::from_secs(15)
            && matches!(
                p.phase,
                LifecyclePhase::Closing { .. } | LifecyclePhase::Replacing { .. }
            )
        {
            self.recovery_degraded_error=Some("Lifecycle outcome pending/indeterminate. Views and frozen buffers retained; Export Copy remains available.".into());
        }
        self.pending_lifecycle = Some(p);
    }
    fn admit_lifecycle(
        &mut self,
        p: &mut PendingLifecycle,
        cx: &mut Context<Self>,
    ) -> Result<(), ServiceError> {
        let recovery = self
            .recovery
            .clone()
            .ok_or_else(|| failure(ErrorKind::Storage, "Recovery service unavailable"))?;
        let revision = self
            .persistence
            .revision
            .ok_or_else(|| failure(ErrorKind::Stale, "Manifest pending"))?;
        for f in &p.frozen {
            let slot = self.resolve_slot(f.target)?;
            if let TabSlot::Editable(ed) = slot {
                if ed.visible_revision != f.revision.unwrap_or(Revision(0))
                    || Some(ed.current_text_seq()) != f.text_seq
                    || ed.has_pending_native_edit(cx)
                    || (ed.meta.encoding, ed.meta.line_ending) != f.format
                {
                    return Err(failure(ErrorKind::Stale, "Frozen document changed"));
                }
            }
        }
        let sequence = self.next_layout_sequence()?;
        match &mut p.kind {
            LifecycleKind::Close => {
                let mut after = self.session.clone();
                after.remove_views(&p.frozen.iter().map(|f| f.target).collect::<Vec<_>>())?;
                let scratch = if after.views.is_empty() {
                    self.untitled_counter += 1;
                    let stub = SessionDocumentStub {
                        doc_id: DocumentId::new(),
                        lifecycle_gen: LifecycleGen(1),
                        durable_rev: Revision(0),
                        meta: DocMeta {
                            title: format!("untitled-{}", self.untitled_counter),
                            ..DocMeta::default()
                        },
                        has_checkpoint: false,
                    };
                    after.insert_view(
                        after.active_pane_id(),
                        LayoutView {
                            id: ViewId::new(),
                            document: stub.key(),
                            group_id: GroupId::UNGROUPED,
                            view: ViewState::default(),
                        },
                        None,
                        true,
                    )?;
                    Some(stub)
                } else {
                    None
                };
                for f in &p.frozen {
                    if matches!(f.disposition, CloseDisposition::KeepRecovery) {
                        if let Some(TabSlot::Editable(ed)) =
                            self.documents.get_mut(&f.target.document.doc_id)
                        {
                            if let Some(commit) = ed.build_recovery_commit() {
                                let rev = commit.new_rev;
                                recovery.try_commit(commit)?;
                                ed.record_commit_queued(rev);
                            }
                        }
                    }
                }
                let entries = p
                    .frozen
                    .iter()
                    .map(|f| CloseEntry {
                        target: f.target,
                        disposition: f.disposition,
                        required_durable_rev: f.revision,
                        expected_format: f.format,
                        view: f.view.clone(),
                        expected_identity: f.identity,
                    })
                    .collect();
                let reply = recovery.close_views(CloseBatch {
                    sequence,
                    expected_revision: revision,
                    entries,
                    after: after.clone(),
                    empty_pane_document: scratch.as_ref().map(|s| DocumentMetaRecord {
                        key: s.key(),
                        meta: s.meta.clone(),
                    }),
                })?;
                p.accepted_revision = Some(revision);
                p.admitted_sequence = Some(sequence);
                p.phase = LifecyclePhase::Closing {
                    reply,
                    after,
                    scratch,
                };
            }
            LifecycleKind::Reload { fresh, staged, .. } => {
                let staged = staged
                    .as_ref()
                    .ok_or_else(|| failure(ErrorKind::InvalidInput, "Disk result not staged"))?;
                let target = p.frozen[0].target;
                let mut after = self.session.clone();
                after.views.get_mut(&target.view_id).unwrap().document = *fresh;
                let tombstone_gen = LifecycleGen(
                    target
                        .document
                        .lifecycle_gen
                        .0
                        .checked_add(1)
                        .ok_or_else(|| failure(ErrorKind::Storage, "Lifecycle exhausted"))?,
                );
                let reply = recovery.replace_view_document(ReloadReplacement {
                    sequence,
                    expected_revision: revision,
                    target,
                    tombstone_gen,
                    replacement: DocumentMetaRecord {
                        key: *fresh,
                        meta: staged.meta.clone(),
                    },
                    after: after.clone(),
                })?;
                p.accepted_revision = Some(revision);
                p.admitted_sequence = Some(sequence);
                p.phase = LifecyclePhase::Replacing { reply, after };
            }
        }
        Ok(())
    }
}

impl EditorWorkspace {
    pub(crate) fn request_recovery_page(
        &mut self,
        cursor: Option<RecoveryCursor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let recovery = self.recovery.clone();
        let database = self.cli.data_dir.join("recovery.sqlite3");
        if recovery.is_none() && !self.recovery_read_only {
            self.recovery_browser.error =
                Some(failure(ErrorKind::Storage, "Recovery storage unavailable"));
            cx.notify();
            return;
        }
        let Some(seq) = self.recovery_browser.request_seq.checked_add(1) else {
            self.recovery_browser.loading = false;
            self.recovery_browser.error = Some(failure(
                ErrorKind::Storage,
                "Recovery request sequence exhausted",
            ));
            cx.notify();
            return;
        };
        self.recovery_browser.request_seq = seq;
        self.recovery_browser.loading = true;
        self.recovery_browser.error = None;
        cx.spawn_in(window, async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    read_catalog_page(
                        recovery.as_ref(),
                        &database,
                        RecoveryPageRequest { cursor, limit: 64 },
                    )
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.recovery_browser.request_seq != seq || !this.recovery_browser.loading {
                    return;
                }
                this.recovery_browser.loading = false;
                match result {
                    Ok(page) => {
                        this.recovery_browser.latest_summary = page.summary.clone();
                        this.recovery_browser.page = Some(page);
                    }
                    Err(e) => this.recovery_browser.error = Some(e),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    pub(crate) fn cancel_recovery_request(&mut self, cx: &mut Context<Self>) {
        if let Some(next) = self.recovery_browser.request_seq.checked_add(1) {
            self.recovery_browser.request_seq = next;
        } else {
            self.recovery_browser.error = Some(failure(
                ErrorKind::Storage,
                "Recovery request sequence exhausted",
            ));
        }
        self.recovery_browser.loading = false;
        if let Some(cancel) = self.recovery_cancel.take() {
            cancel.cancel();
        }
        cx.notify();
    }
    pub(crate) fn open_recovery_entry(
        &mut self,
        key: RecoveryEntryKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy() {
            self.notify_error(
                failure(ErrorKind::Busy, "Lifecycle operation pending"),
                window,
                cx,
            );
            return;
        }
        if key.origin == RecoveryEntryOrigin::Document
            && let (Some(doc_id), Some(lifecycle_gen)) = (key.doc_id, key.lifecycle_gen)
        {
            let document = DocumentKey {
                doc_id,
                lifecycle_gen,
            };
            if let Some(target) = self
                .session
                .views
                .values()
                .find(|view| view.document == document)
                .map(|view| ViewTarget {
                    view_id: view.id,
                    document,
                })
            {
                self.activate_view(target, window, cx);
                if self.active_view_id() == Some(target.view_id) {
                    self.close_recovery_browser_on_open(cx);
                }
                return;
            }
        }
        let Some(recovery) = self.recovery.clone() else {
            self.notify_error(failure(ErrorKind::Storage,"Recovery is read-only or unavailable; Export Copy remains available for stored drafts"),window,cx);
            return;
        };
        let database = self.cli.data_dir.join("recovery.sqlite3");
        cx.spawn_in(window, async move |weak, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { read_catalog_entry(Some(&recovery), &database, key) })
                .await;
            let _ = weak.update_in(cx, |this, w, c| match result {
                Ok(entry) => {
                    if this.is_lifecycle_busy() {
                        this.notify_error(
                            failure(
                                ErrorKind::Busy,
                                "Lifecycle operation pending; recovery entry retained",
                            ),
                            w,
                            c,
                        );
                        return;
                    }
                    let stub = match entry.stub {
                        Ok(s) => s,
                        Err(e) => {
                            this.notify_error(e, w, c);
                            return;
                        }
                    };
                    if let Some(target) = this
                        .session
                        .views
                        .values()
                        .find(|v| v.document == stub.key())
                        .map(|v| ViewTarget {
                            view_id: v.id,
                            document: v.document,
                        })
                    {
                        this.activate_view(target, w, c);
                        if this.active_view_id() == Some(target.view_id) {
                            this.close_recovery_browser_on_open(c);
                        }
                        return;
                    }
                    this.admit_recovered_view(
                        stub,
                        entry.last_placement,
                        entry.category == editor_services::RecoveryEntryCategory::Closed,
                        w,
                        c,
                    );
                }
                Err(e) => this.notify_error(e, w, c),
            });
        })
        .detach();
    }
    fn admit_recovered_view(
        &mut self,
        stub: SessionDocumentStub,
        placement: Option<ClosedViewRecord>,
        closed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy() || self.persistence.failed || self.recovery.is_none() {
            self.notify_error(failure(ErrorKind::Storage,"Opening a stored draft requires acknowledged session persistence; use Export Copy while storage is unavailable"),window,cx);
            return;
        }
        if self.session.views.len() >= MAX_SESSION_VIEWS {
            self.notify_error(failure(ErrorKind::InvalidInput,"Close a view while keeping its stored draft before opening another; the 1024-view limit does not limit browsing or Export Copy"),window,cx);
            return;
        }
        let existing_group = placement
            .as_ref()
            .map(|p| p.previous_view.group_id)
            .filter(|id| self.session.group(*id).is_some());
        let group = existing_group.unwrap_or_else(|| {
            if placement.is_some() {
                GroupId::UNGROUPED
            } else {
                self.session.active_group()
            }
        });
        let position = existing_group.and_then(|_| placement.as_ref().map(|p| p.position as usize));
        let view = placement
            .as_ref()
            .map(|p| p.previous_view.view.clone())
            .unwrap_or_default();
        let target = ViewTarget {
            view_id: ViewId::new(),
            document: stub.key(),
        };
        let pane = placement
            .as_ref()
            .map(|p| p.pane_id)
            .filter(|id| self.session.pane(*id).is_some())
            .unwrap_or(self.active_pane_id());
        if let Err(e) = self.session.insert_view(
            pane,
            LayoutView {
                id: target.view_id,
                document: target.document,
                group_id: group,
                view,
            },
            position,
            false,
        ) {
            self.notify_error(e, window, cx);
            return;
        }
        self.documents
            .insert(stub.doc_id, TabSlot::Dehydrated(stub));
        if closed {
            self.mark_metadata(target.document.doc_id, PendingDocChangeKind::Reopen);
        } else {
            self.persistence.structural = true;
            self.layout_dirty = true;
        }
        self.recently_closed
            .retain(|r| r.document.key() != target.document);
        self.activate_view(target, window, cx);
        if self.active_view_id() == Some(target.view_id) {
            self.close_recovery_browser_on_open(cx);
        }
    }
    pub fn reopen_recently_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_lifecycle_busy() {
            return;
        }
        if let Some(record) = self.recently_closed.first().cloned() {
            self.admit_recovered_view(record.document.clone(), Some(record), true, window, cx);
        } else {
            self.status_flash = Some(("No recently closed views".into(), Instant::now()));
            cx.notify();
        }
    }
    pub(crate) fn export_recovery_entry(
        &mut self,
        key: RecoveryEntryKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_export.is_some() {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "One selected draft export is already running",
                ),
                window,
                cx,
            );
            return;
        }
        let recovery = self.recovery.clone();
        let database = self.cli.data_dir.join("recovery.sqlite3");
        if recovery.is_none() && !self.recovery_read_only {
            self.notify_error(
                failure(ErrorKind::Storage, "Recovery storage unavailable"),
                window,
                cx,
            );
            return;
        }
        let sequence = match self.next_operation() {
            Ok(sequence) => sequence,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        self.draft_export = Some(sequence);
        let cancel = CancelToken::new();
        self.recovery_cancel = Some(cancel.clone());
        self.recovery_browser.active_export = Some(key);
        let prompt = cx.prompt_for_new_path(&PathBuf::from("."), Some("recovered-draft.txt"));
        let faults = self.faults.clone();
        let paging = self.paging_service.clone();
        cx.spawn_in(window,async move |weak,cx|{
            let selected=prompt.await;
            let result=if let Ok(Ok(Some(path)))=selected{
                cx.background_executor().spawn(async move{
                    if cancel.is_cancelled(){return Err(failure(ErrorKind::Cancelled,"Export cancelled before publication"));}
                    let entry=read_catalog_entry(recovery.as_ref(),&database,key)?;let stub=entry.stub?;
                    let rope=if stub.has_checkpoint{hydrate_from_source(recovery.as_ref(),&database,stub.key(),&cancel)?.rope}
                        else if let Some(source)=&stub.meta.path{match open_document(stub.doc_id,source,&paging,OpenOptions::default(),&cancel)?{OpenOutcome::Editable{rope,..}=>rope,OpenOutcome::Paged{..}=>return Err(failure(ErrorKind::Unsupported,"This disk-backed record exceeds the 64 MiB draft export hydration cap; copy the original disk file instead"))}}
                        else{Rope::new()};
                    if cancel.is_cancelled(){return Err(failure(ErrorKind::Cancelled,"Export cancelled before publication"));}
                    let request=SaveRequest{doc_id:stub.doc_id,lifecycle_gen:stub.lifecycle_gen,captured_rev:stub.durable_rev,rope,target_path:path,encoding:stub.meta.encoding,line_ending:stub.meta.line_ending,expected_fingerprint:None,force_overwrite:true};
                    Ok(save_document_copy(&request,&faults))
                }).await
            }else{Err(failure(ErrorKind::Cancelled,"Export destination selection cancelled"))};
            let _=weak.update_in(cx,|this,w,c|{
                if this.draft_export==Some(sequence){this.draft_export=None;}
                if this.recovery_browser.active_export==Some(key){this.recovery_browser.active_export=None;this.recovery_cancel=None;}
                // A completed publication is reported even if the browser closed or its query changed.
                this.report_copy_result(result,w,c);
            });
        }).detach();
        cx.notify();
    }
    pub(crate) fn export_view_draft_copy(
        &mut self,
        target: ViewTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.draft_export.is_some() {
            self.notify_error(
                failure(
                    ErrorKind::Busy,
                    "One selected draft export is already running",
                ),
                window,
                cx,
            );
            return;
        }
        let slot = match self.resolve_slot(target) {
            Ok(s) => s,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        let request = if let TabSlot::Editable(ed) = slot {
            if ed.has_pending_native_edit(cx) {
                self.notify_error(
                    failure(ErrorKind::Busy, "Pending native edit; retry Export Copy"),
                    window,
                    cx,
                );
                return;
            }
            Some(SaveRequest {
                doc_id: ed.doc_id,
                lifecycle_gen: ed.lifecycle_gen,
                captured_rev: ed.visible_revision,
                rope: ed.committed_rope().clone(),
                target_path: PathBuf::new(),
                encoding: ed.meta.encoding,
                line_ending: ed.meta.line_ending,
                expected_fingerprint: None,
                force_overwrite: true,
            })
        } else {
            None
        };
        let stub = match slot {
            TabSlot::Dehydrated(s)
            | TabSlot::Hydrating { stub: s, .. }
            | TabSlot::OverBudgetDraft { stub: s, .. }
            | TabSlot::Failed { stub: s, .. } => Some(s.clone()),
            _ => None,
        };
        if request.is_none() && stub.as_ref().is_none_or(|s| !s.has_checkpoint) {
            self.notify_error(
                failure(
                    ErrorKind::Unsupported,
                    "No hydrated or stored draft to export; the disk source is unchanged",
                ),
                window,
                cx,
            );
            return;
        }
        let sequence = match self.next_operation() {
            Ok(sequence) => sequence,
            Err(e) => {
                self.notify_error(e, window, cx);
                return;
            }
        };
        self.draft_export = Some(sequence);
        let cancel = CancelToken::new();
        self.recovery_cancel = Some(cancel.clone());
        let prompt = cx.prompt_for_new_path(&PathBuf::from("."), Some("draft-copy.txt"));
        let faults = self.faults.clone();
        let recovery = self.recovery.clone();
        let database = self.cli.data_dir.join("recovery.sqlite3");
        let read_only = self.recovery_read_only;
        cx.spawn_in(window, async move |weak, cx| {
            let result = match prompt.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            if cancel.is_cancelled() {
                                return Err(failure(
                                    ErrorKind::Cancelled,
                                    "Copy cancelled before publication",
                                ));
                            }
                            let mut request = if let Some(request) = request {
                                request
                            } else {
                                let stub = stub
                                    .ok_or_else(|| failure(ErrorKind::InvalidInput, "No draft"))?;
                                if recovery.is_none() && !read_only {
                                    return Err(failure(
                                        ErrorKind::Storage,
                                        "Recovery unavailable",
                                    ));
                                }
                                let draft = hydrate_from_source(
                                    recovery.as_ref(),
                                    &database,
                                    stub.key(),
                                    &cancel,
                                )?;
                                SaveRequest {
                                    doc_id: draft.doc_id,
                                    lifecycle_gen: draft.lifecycle_gen,
                                    captured_rev: draft.durable_rev,
                                    rope: draft.rope,
                                    target_path: PathBuf::new(),
                                    encoding: draft.meta.encoding,
                                    line_ending: draft.meta.line_ending,
                                    expected_fingerprint: None,
                                    force_overwrite: true,
                                }
                            };
                            if cancel.is_cancelled() {
                                return Err(failure(
                                    ErrorKind::Cancelled,
                                    "Copy cancelled before publication",
                                ));
                            }
                            request.target_path = path;
                            Ok(save_document_copy(&request, &faults))
                        })
                        .await
                }
                _ => Err(failure(
                    ErrorKind::Cancelled,
                    "Copy destination selection cancelled",
                )),
            };
            let _ = weak.update_in(cx, |this, w, c| {
                if this.draft_export == Some(sequence) {
                    this.draft_export = None;
                    this.recovery_cancel = None;
                }
                this.report_copy_result(result, w, c);
                c.notify();
            });
        })
        .detach();
    }
    fn report_copy_result(
        &self,
        result: Result<SaveOutcome, ServiceError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(SaveOutcome::Saved { .. }) => window.push_notification(
                Notification::success(
                    "Exported copy; original document association and recovery draft unchanged",
                ),
                cx,
            ),
            Ok(SaveOutcome::Failed { error, .. }) => self.notify_error(error, window, cx),
            Ok(SaveOutcome::ConflictDetected { .. }) => self.notify_error(
                failure(
                    ErrorKind::Conflict,
                    "Export target changed; copy was not published",
                ),
                window,
                cx,
            ),
            Ok(SaveOutcome::PublishedNotDurable { warning, .. }) => self.notify_error(
                failure(
                    ErrorKind::PublishedNotDurable,
                    format!(
                        "Copy published, durability unconfirmed: {}",
                        warning.message
                    ),
                ),
                window,
                cx,
            ),
            Ok(SaveOutcome::SavedWithRecoveryWarning { warning, .. }) => {
                self.notify_error(warning, window, cx)
            }
            Err(e) if e.kind == ErrorKind::Cancelled => {}
            Err(e) => self.notify_error(e, window, cx),
        }
    }
}
fn read_catalog_page(
    recovery: Option<&RecoveryHandle>,
    database: &Path,
    request: RecoveryPageRequest,
) -> Result<RecoveryPage, ServiceError> {
    match recovery {
        Some(recovery) => recovery
            .request_recovery_page(request)?
            .recv()
            .map_err(|_| disconnected())?,
        None => editor_services::read_recovery_page(database, request),
    }
}
fn read_catalog_entry(
    recovery: Option<&RecoveryHandle>,
    database: &Path,
    key: RecoveryEntryKey,
) -> Result<editor_services::RecoveryEntry, ServiceError> {
    let cursor = RecoveryCursor {
        origin: key.origin,
        after_rowid: key.rowid.checked_sub(1),
        document_upper_rowid: (key.origin == RecoveryEntryOrigin::Document).then_some(key.rowid),
        legacy_orphan_upper_rowid: (key.origin == RecoveryEntryOrigin::LegacyOrphan)
            .then_some(key.rowid),
    };
    let page = read_catalog_page(
        recovery,
        database,
        RecoveryPageRequest {
            cursor: Some(cursor),
            limit: 1,
        },
    )?;
    page.entries
        .into_iter()
        .find(|entry| entry.key == key)
        .ok_or_else(|| {
            failure(
                ErrorKind::Stale,
                "Recovery entry changed or was discarded; refresh the browser",
            )
        })
}
fn hydrate_from_source(
    recovery: Option<&RecoveryHandle>,
    database: &Path,
    key: DocumentKey,
    cancel: &CancelToken,
) -> Result<HydratedDraft, ServiceError> {
    if let Some(recovery) = recovery {
        let reply = loop {
            if cancel.is_cancelled() {
                return Err(failure(ErrorKind::Cancelled, "operation cancelled"));
            }
            match recovery.request_hydrate(key, cancel.clone()) {
                Err(e) if e.kind == ErrorKind::Busy => {
                    std::thread::sleep(Duration::from_millis(15))
                }
                result => break result?,
            }
        };
        reply.recv().map_err(|_| disconnected())?
    } else {
        editor_services::read_recovery_draft(database, key, cancel)
    }
}

impl EditorWorkspace {
    pub(super) fn prepare_quit_barrier(
        &mut self,
        force: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.cancel_pane_resize(cx);
        if self.shutting_down {
            return true;
        }
        if self.is_lifecycle_busy()
            || !self.pending_open_paths.is_empty()
            || !self.pending_saves.is_empty()
            || self.draft_export.is_some()
            || !self.recovery_ready
        {
            self.pending_quit = Some(force);
            return false;
        }
        if self
            .documents
            .values()
            .any(|s| matches!(s,TabSlot::Editable(ed) if ed.has_pending_native_edit(cx)))
        {
            self.pending_quit = Some(force);
            return false;
        }
        self.capture_session_view_states(cx);
        if self.persistence.inflight.is_some()
            || (self.layout_dirty && self.recovery.is_some() && !self.persistence.failed)
        {
            self.pending_quit = Some(force);
            return false;
        }
        if self.persistence.failed && !force {
            self.pending_quit = None;
            self.open_degraded_quit_dialog("Session persistence failed; remain open and export drafts or explicitly quit without recovery",window,cx);
            return false;
        }
        true
    }
}

fn open_disk_for_budget(
    doc_id: DocumentId,
    path: &Path,
    paging: &PagingService,
    mut options: OpenOptions,
    force_paged: bool,
    cancel: &CancelToken,
) -> Result<OpenOutcome, ServiceError> {
    if force_paged {
        options.editable_byte_limit = 0;
    }
    let outcome = loop {
        if cancel.is_cancelled() {
            return Err(failure(ErrorKind::Cancelled, "operation cancelled"));
        }
        match open_document(doc_id, path, paging, options, cancel) {
            Err(error) if error.kind == ErrorKind::Busy => {
                std::thread::sleep(Duration::from_millis(15))
            }
            result => break result?,
        }
    };
    if !force_paged {
        return Ok(outcome);
    }
    match outcome {
        OpenOutcome::Editable {
            encoding,
            line_ending,
            fingerprint,
            ..
        } => {
            if cancel.is_cancelled() {
                return Err(failure(ErrorKind::Cancelled, "Disk open cancelled"));
            }
            let doc = paging.open(doc_id, path)?;
            if doc.fingerprint() != &fingerprint {
                return Err(failure(
                    ErrorKind::SourceChanged,
                    "Source changed while staging paged admission",
                ));
            }
            let source_gen = doc.source_generation();
            Ok(OpenOutcome::Paged {
                doc,
                encoding,
                line_ending,
                fingerprint,
                source_gen,
                readonly_reason: editor_services::ReadOnlyReason::LargeFile,
            })
        }
        paged => Ok(paged),
    }
}

impl Drop for EditorWorkspace {
    fn drop(&mut self) {
        for (_, cancel) in self.hydration_requests.values() {
            cancel.cancel();
        }
        if let Some(cancel) = &self.recovery_cancel {
            cancel.cancel();
        }
        if let Some(pending) = &self.pending_lifecycle {
            pending.cancel.cancel();
        }
        self.search.cancel_active();
    }
}
