use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use editor_services::{
    CancelToken, ClosedViewRecord, ContentIdentity, DocMeta, DocumentId, DocumentKey, EncodingSpec,
    ErrorKind, FaultInjector, GroupId, HydratedDraft, IndexStatus, LifecycleGen, LineEnding,
    OpenOptions, OpenOutcome, PagingService, RecoveryEvent, RecoveryHandle, Revision, SaveOutcome,
    SaveRequest, SearchService, SearchTarget, ServiceError, SessionDocumentStub, SessionLayout,
    SessionManifest, ShellTheme, TryRecvError, UiShellLoad, UiShellState, UiShellStore, ViewId,
    ViewState, ViewTarget, ViewportAnchor, open_document, save_document,
};
#[path = "session_core.rs"]
mod session_core;
use crate::group_ui::GroupUiState;
use gpui_kit::component::button::{ButtonVariant, ButtonVariants as _};
use gpui_kit::component::command::CommandState;
use gpui_kit::component::dialog::DialogButtonProps;
use gpui_kit::component::input::{
    Enter, Escape, Indent, InputEditsEvent, InputEvent, InputState, Outdent, SelectAll,
};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Theme, ThemeMode, VirtualListScrollHandle, WindowExt as _,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, ExternalPaths, FocusHandle, Focusable,
    ParentElement as _, PathPromptOptions, Size, Styled as _, Subscription, Window, px, size,
};
use ropey::Rope;
use session_core::*;

use crate::actions::{
    CancelActiveSearch, ClearRecentFiles, CloseActiveTab, CycleLanguage, CycleSearchKernel,
    DeleteLine, DuplicateLine, FindNextMatch, FindPrevMatch, MoveLineDown, MoveLineUp, NewTab,
    NextTab, OpenCommandPalette, OpenFile, OpenGoToOffset, OpenQuickSwitcher, PrevTab, QuitEditor,
    ReloadActiveDocument, ReopenClosedTab, SaveFile, SaveFileAs, SelectLangBash, SelectLangC,
    SelectLangCpp, SelectLangCss, SelectLangGo, SelectLangHtml, SelectLangJavascript,
    SelectLangJson, SelectLangMake, SelectLangMarkdown, SelectLangPlaintext, SelectLangPython,
    SelectLangRust, SelectLangSql, SelectLangSwift, SelectLangToml, SelectLangTsx,
    SelectLangTypescript, SelectLangXml, SelectLangYaml, SelectLanguageAuto, SetEncodingUtf8,
    SetEncodingUtf8Bom, SetIndentSpaces2, SetIndentSpaces4, SetIndentSpaces8, SetIndentTabs2,
    SetIndentTabs4, SetIndentTabs8, SetLineEndingCrlf, SetLineEndingLf, ToggleComment,
    ToggleLineEnding, ToggleReplace, ToggleSearch, ToggleShowWhitespace, ToggleSidebar,
    ToggleSoftWrap, ToggleThemeMode, ToggleUtf8Bom,
};
use crate::budget::{
    AdmissionDecision, HydratedSlotInfo, HydrationBudgetConfig, plan_hydration_admission,
};
use crate::cli::{CliArgs, InitialTheme};
use crate::editor_adapter::{
    ApplyEditsError, CommittedMutationOutcome, EditorAdapter, FormatChangeOutcome,
    HydrateEditorParams, MAX_ORDINARY_DOC_BYTES, PendingSavepointCheck,
};
use crate::language::next_language;
use crate::line_ops::{LineOpKind, LineOpPlan, plan_line_op, plan_toggle_comment_for_language};
use crate::paged_view::{PagedTabState, format_bytes, parse_jump_query};
use crate::search_bar::{
    SearchQueryKind, SearchSessionState, SearchStatus, plan_replacement_edits,
    run_editable_extended_search,
};
use crate::shell::{
    EarlyShellOverrides, EditorViewPrefs, HonestSlotBadge, PaletteCommandTarget, PaletteMode,
    describe_unhydrated_stub_badge, merge_loaded_shell_state,
};
pub(crate) enum TabSlot {
    Dehydrated(SessionDocumentStub),
    Hydrating {
        stub: SessionDocumentStub,
        req_seq: u64,
    },
    OverBudgetDraft {
        stub: SessionDocumentStub,
        reason: String,
    },
    Editable(EditorAdapter),
    Paged(PagedTabState),
    Failed {
        stub: SessionDocumentStub,
        error: String,
    },
}
impl TabSlot {
    pub(crate) fn doc_id(&self) -> DocumentId {
        match self {
            Self::Editable(ed) => ed.doc_id,
            Self::Paged(pv) => pv.doc_id,
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. }
            | Self::Failed { stub: s, .. } => s.doc_id,
        }
    }
    pub(crate) fn lifecycle_gen(&self) -> LifecycleGen {
        match self {
            Self::Editable(ed) => ed.lifecycle_gen,
            Self::Paged(pv) => pv.lifecycle_gen,
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. }
            | Self::Failed { stub: s, .. } => s.lifecycle_gen,
        }
    }
    pub(crate) fn document_key(&self) -> DocumentKey {
        DocumentKey {
            doc_id: self.doc_id(),
            lifecycle_gen: self.lifecycle_gen(),
        }
    }
    pub(crate) fn meta(&self) -> &DocMeta {
        match self {
            Self::Editable(ed) => &ed.meta,
            Self::Paged(pv) => &pv.meta,
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. }
            | Self::Failed { stub: s, .. } => &s.meta,
        }
    }
    pub(crate) fn title(&self) -> &str {
        let title = self.meta().title.as_str();
        if title.is_empty() { "untitled" } else { title }
    }
    pub(crate) fn has_checkpoint(&self) -> bool {
        match self {
            Self::Editable(ed) => ed.has_checkpoint,
            Self::Paged(_) => false,
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. }
            | Self::Failed { stub: s, .. } => s.has_checkpoint,
        }
    }
    pub(crate) fn proven_modified(&self) -> bool {
        self.honest_badge().proven_modified
    }
    pub(super) fn close_guard(&self) -> bool {
        match self {
            Self::Editable(ed) => ed.is_dirty(),
            Self::Paged(_) => false,
            _ => self.has_checkpoint() || self.proven_modified(),
        }
    }
    pub(crate) fn honest_badge(&self) -> HonestSlotBadge {
        match self {
            Self::Editable(ed) => HonestSlotBadge {
                short_badge: if ed.is_dirty() {
                    Some("MOD")
                } else if ed.is_readonly() {
                    Some("RO")
                } else {
                    None
                },
                detail_hint: if ed.is_dirty() {
                    Some("Modified")
                } else if ed.is_readonly() {
                    Some("Read-only")
                } else {
                    None
                },
                proven_modified: ed.is_dirty(),
            },
            Self::Paged(_) => HonestSlotBadge {
                short_badge: Some("PG"),
                detail_hint: Some("Paged read-only"),
                proven_modified: false,
            },
            Self::Dehydrated(stub) => {
                describe_unhydrated_stub_badge(&stub.meta, stub.has_checkpoint)
            }
            Self::Hydrating { stub, .. }
            | Self::OverBudgetDraft { stub, .. }
            | Self::Failed { stub, .. } => {
                let base = describe_unhydrated_stub_badge(&stub.meta, stub.has_checkpoint);
                let (short_badge, detail_hint) = match self {
                    Self::Hydrating { .. } => ("…", "Loading…"),
                    Self::OverBudgetDraft { .. } => ("CAP", "Protected over-budget draft"),
                    _ => ("ERR", "Load error; stored recovery facts preserved"),
                };
                HonestSlotBadge {
                    short_badge: Some(short_badge),
                    detail_hint: Some(detail_hint),
                    ..base
                }
            }
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayFocus {
    Editor,
    SearchQuery,
    SearchReplace,
    Workspace,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlayOrigin {
    pub(crate) view: Option<ViewTarget>,
    pub(crate) focus: OverlayFocus,
}

#[derive(Clone, Debug, Default)]
pub struct StartupMetrics {
    pub process_started_ms_to_window: f64,
    pub first_interactive_ms: Option<f64>,
    pub recovery_manifest_ms: Option<f64>,
}

pub struct EditorWorkspace {
    pub(crate) cli: CliArgs,
    pub(crate) process_start: Instant,
    pub(crate) startup_metrics: StartupMetrics,

    pub(crate) focus_handle: FocusHandle,
    pub(crate) paging_service: PagingService,
    pub(crate) search_service: SearchService,
    pub(crate) recovery: Option<RecoveryHandle>,
    pub(crate) recovery_degraded_error: Option<String>,
    pub(crate) recovery_ready: bool,
    bootstrap_indeterminate: bool,
    recovery_read_only: bool,
    pub(crate) faults: FaultInjector,

    pub(crate) budget_config: HydrationBudgetConfig,
    pub(crate) activation_clock: u64,
    pub(crate) untitled_counter: usize,

    pub(crate) session: SessionLayout,
    pub(crate) documents: HashMap<DocumentId, TabSlot>,
    pub(crate) group_ui: GroupUiState,
    recently_closed: Vec<ClosedViewRecord>,
    persistence: SessionPersistence,
    pending_lifecycle: Option<PendingLifecycle>,
    pending_saves: HashMap<ViewTarget, u64>,
    operation_sequence: u64,
    pending_quit: Option<bool>,
    pending_open_paths: VecDeque<PathBuf>,
    pending_open_group: Option<GroupId>,
    recovery_browser: RecoveryBrowserState,
    recovery_cancel: Option<CancelToken>,
    draft_export: Option<u64>,
    hydration_requests: HashMap<ViewTarget, (u64, CancelToken)>,
    deferred_completions: Vec<DeferredCompletion>,

    pub(crate) search: SearchSessionState,
    pub(crate) search_query_input: gpui_kit::Entity<InputState>,
    pub(crate) search_replace_input: gpui_kit::Entity<InputState>,
    pub(crate) search_results_scroll: VirtualListScrollHandle,
    pub(crate) search_item_sizes: Rc<Vec<Size<gpui_kit::Pixels>>>,

    pub(crate) goto_visible: bool,
    pub(crate) goto_input: gpui_kit::Entity<InputState>,

    pub(crate) shell_store: Option<UiShellStore>,
    pub(crate) shell_loaded: bool,
    pub(crate) shell_state: UiShellState,
    pub(crate) shell_early_overrides: EarlyShellOverrides,
    pub(crate) shell_early_recent: Vec<PathBuf>,
    pub(crate) shell_pending_queue: bool,
    pub(crate) shell_flush_in_flight: bool,
    pub(crate) shell_persistence_warning: Option<String>,

    pub(crate) sidebar_open_scroll: VirtualListScrollHandle,
    pub(crate) sidebar_recent_scroll: VirtualListScrollHandle,

    pub(crate) palette_state: gpui_kit::Entity<CommandState>,
    pub(crate) palette_visible: bool,
    pub(crate) palette_base_mode: PaletteMode,
    pub(crate) palette_prior_origin: Option<OverlayOrigin>,
    pub(crate) palette_targets: Vec<(IndexPath, PaletteCommandTarget)>,

    pub(crate) status_flash: Option<(String, Instant)>,
    pub(crate) last_edit_instant: Option<Instant>,
    pub(crate) shutting_down: bool,
    pub(crate) quit_confirmed: bool,
    pub shutdown_durable_ack: Arc<AtomicBool>,
    pub(crate) layout_dirty: bool,
    pub(crate) paged_indexer_running: bool,
    pub(crate) paged_index_rr_cursor: usize,
    pub(crate) smoke_outcome: Option<crate::smoke::SharedSmokeOutcome>,

    _ui_subscriptions: Vec<Subscription>,
}

impl Focusable for EditorWorkspace {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EditorWorkspace {
    pub fn new(
        cli: CliArgs,
        process_start: Instant,
        smoke_outcome: Option<crate::smoke::SharedSmokeOutcome>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        match cli.theme {
            InitialTheme::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            InitialTheme::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
            InitialTheme::System => Theme::sync_system_appearance(Some(window), cx),
        }

        let focus_handle = cx.focus_handle();
        let search_query_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Find text or pattern…"));
        let search_replace_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Replace with…"));
        let goto_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Line (:120) or 64-bit byte offset (b:1048576 / 0x100000)…")
        });
        let palette_state = cx.new(|cx| CommandState::new(window, cx));
        let mut ui_subscriptions = Vec::new();

        ui_subscriptions.push(cx.subscribe_in(
            &search_query_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    this.run_search_from_inputs(window, cx);
                }
                InputEvent::PressEnter { shift, .. } => {
                    if *shift {
                        this.on_find_prev(&FindPrevMatch, window, cx);
                    } else {
                        this.on_find_next(&FindNextMatch, window, cx);
                    }
                }
                _ => {}
            },
        ));
        ui_subscriptions.push(cx.subscribe_in(
            &search_replace_input,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.replace_current_match(window, cx);
                }
            },
        ));
        ui_subscriptions.push(cx.subscribe_in(
            &goto_input,
            window,
            |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.execute_goto_from_input(window, cx);
                }
            },
        ));

        let window_ms = process_start.elapsed().as_secs_f64() * 1000.0;
        let budget_config = HydrationBudgetConfig {
            max_hydrated_bytes: cli.max_hydrated_bytes,
            soft_clean_editor_target: cli.soft_clean_editor_target,
            max_hydrated_editors: cli.max_hydrated_editors,
        };

        let mut workspace = Self {
            cli,
            process_start,
            startup_metrics: StartupMetrics {
                process_started_ms_to_window: window_ms,
                first_interactive_ms: None,
                recovery_manifest_ms: None,
            },
            focus_handle,
            paging_service: PagingService::default(),
            search_service: SearchService::default(),
            recovery: None,
            recovery_degraded_error: None,
            recovery_ready: false,
            bootstrap_indeterminate: false,
            recovery_read_only: false,
            faults: FaultInjector::default(),
            budget_config,
            activation_clock: 1,
            untitled_counter: 0,
            session: SessionLayout::default(),
            documents: HashMap::new(),
            group_ui: GroupUiState::default(),
            recently_closed: Vec::new(),
            persistence: SessionPersistence::default(),
            pending_lifecycle: None,
            pending_saves: HashMap::new(),
            operation_sequence: 0,
            pending_quit: None,
            pending_open_paths: VecDeque::new(),
            pending_open_group: None,
            recovery_browser: RecoveryBrowserState::default(),
            recovery_cancel: None,
            draft_export: None,
            hydration_requests: HashMap::new(),
            deferred_completions: Vec::new(),
            search: SearchSessionState::default(),
            search_query_input,
            search_replace_input,
            search_results_scroll: VirtualListScrollHandle::new(),
            search_item_sizes: Rc::new(Vec::new()),
            goto_visible: false,
            goto_input,
            shell_store: None,
            shell_loaded: false,
            shell_state: UiShellState::default(),
            shell_early_overrides: EarlyShellOverrides::default(),
            shell_early_recent: Vec::new(),
            shell_pending_queue: false,
            shell_flush_in_flight: false,
            shell_persistence_warning: None,
            sidebar_open_scroll: VirtualListScrollHandle::new(),
            sidebar_recent_scroll: VirtualListScrollHandle::new(),
            palette_state,
            palette_visible: false,
            palette_base_mode: PaletteMode::Commands,
            palette_prior_origin: None,
            palette_targets: Vec::new(),
            status_flash: None,
            last_edit_instant: None,
            shutting_down: false,
            quit_confirmed: false,
            shutdown_durable_ack: Arc::new(AtomicBool::new(false)),
            layout_dirty: false,
            paged_indexer_running: false,
            paged_index_rr_cursor: 0,
            smoke_outcome,
            _ui_subscriptions: ui_subscriptions,
        };

        // Create an immediate interactive scratch tab ONLY if no CLI files exist yet; if recovery
        // manifest arrives while this initial scratch tab is untouched, we seamlessly replace it
        // with the recovered session so startup is immediately interactive (`§13.1`).
        workspace.create_untitled_tab(false, window, cx);
        workspace.startup_metrics.first_interactive_ms =
            Some(process_start.elapsed().as_secs_f64() * 1000.0);

        workspace.install_window_close_guard(window, cx);
        workspace.start_background_bootstrap(window, cx);
        workspace.start_background_pump(window, cx);

        workspace
    }

    fn install_window_close_guard(&self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let Some(entity) = weak.upgrade() else {
                return true;
            };
            let allow_now = entity.read(cx).quit_confirmed;
            if allow_now {
                return true;
            }
            entity.update(cx, |this, cx| {
                this.initiate_clean_quit(false, window, cx);
            });
            false
        });
    }

    fn start_background_pump(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |weak, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                let keep_going = weak
                    .update_in(cx, |this, window, cx| {
                        this.tick_background(window, cx);
                        !this.quit_confirmed
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    fn tick_background(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dirty_ui = false;

        // 1. Drain RecoveryEvents without blocking UI.
        if let Some(recovery) = self.recovery.clone() {
            let rx = recovery.events();
            for _ in 0..64 {
                match rx.try_recv() {
                    Ok(RecoveryEvent::DurableAck {
                        doc_id,
                        lifecycle_gen,
                        durable_rev,
                    }) => {
                        if let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) {
                            ed.mark_durable_ack(lifecycle_gen, durable_rev);
                            dirty_ui = true;
                        }
                    }
                    Ok(RecoveryEvent::StorageError { error, .. }) => {
                        let msg = error.message;
                        if self.recovery_degraded_error.as_deref() != Some(msg.as_str()) {
                            self.recovery_degraded_error = Some(msg.clone());
                            window.push_notification(
                                Notification::warning(format!(
                                    "Draft recovery storage error: {msg}"
                                )),
                                cx,
                            );
                            dirty_ui = true;
                        }
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }

            // 2. Coalesced 250ms / baseline flush of pending commits.
            let due = self
                .last_edit_instant
                .is_none_or(|t| t.elapsed() >= Duration::from_millis(220));
            if !self.is_lifecycle_busy() {
                let persistence = &self.persistence;
                for slot in self.documents.values_mut() {
                    let TabSlot::Editable(ed) = slot else {
                        continue;
                    };
                    if !persistence.allows_commit(ed.doc_id) {
                        continue;
                    }
                    if ed.visible_revision <= ed.queued_revision {
                        continue;
                    }
                    if ed.committed_byte_len() > MAX_ORDINARY_DOC_BYTES as u64 {
                        let msg = "Buffer exceeds 64 MiB automatic recovery cap; use Save / Save As… to persist changes to disk.".to_string();
                        if self.recovery_degraded_error.as_deref() != Some(msg.as_str()) {
                            self.recovery_degraded_error = Some(msg.clone());
                            dirty_ui = true;
                        }
                        if !ed.oversized_recovery_warned {
                            ed.oversized_recovery_warned = true;
                            window.push_notification(Notification::warning(msg), cx);
                            dirty_ui = true;
                        }
                        continue;
                    }
                    let urgent = ed.durable_revision.0 == 0;
                    if !due && !urgent {
                        continue;
                    }
                    if let Some(commit) = ed.build_recovery_commit() {
                        let target_rev = commit.new_rev;
                        match recovery.try_commit(commit) {
                            Ok(()) => {
                                ed.record_commit_queued(target_rev);
                                if self.recovery_degraded_error.is_some() {
                                    self.recovery_degraded_error = None;
                                }
                                dirty_ui = true;
                            }
                            Err(err) if err.kind == ErrorKind::Busy => {
                                ed.record_commit_busy();
                            }
                            Err(err) => {
                                self.recovery_degraded_error = Some(err.message);
                                dirty_ui = true;
                            }
                        }
                    }
                }
            }
        }

        self.pump_session(window, cx);
        if self.shell_pending_queue && !self.shutting_down {
            self.queue_shell_persistence(window, cx);
        }
        // 3. Poll streaming search batches.
        if self.search.try_flush_pending_start(&self.search_service) {
            dirty_ui = true;
        }
        let prev_count = self.search.matches.len();
        if self.search.poll_streaming_events(32) {
            if self.search.matches.len() != prev_count {
                self.rebuild_search_item_sizes();
            }
            self.sync_active_search_decorations(cx);
            dirty_ui = true;
        }

        // 4. Expire transient status flash.
        if let Some((_, ts)) = &self.status_flash
            && ts.elapsed() >= Duration::from_secs(4)
        {
            self.status_flash = None;
            dirty_ui = true;
        }

        if dirty_ui {
            cx.notify();
        }
    }

    pub(crate) fn rebuild_search_item_sizes(&mut self) {
        let row_size = size(px(1200.), px(24.));
        self.search_item_sizes = Rc::new(vec![row_size; self.search.matches.len()]);
    }

    pub(super) fn subscribe_editor(
        &self,
        adapter: &EditorAdapter,
        target: ViewTarget,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(
            adapter.editor(),
            move |this: &mut Self, _editor, event: &InputEditsEvent, cx| {
                this.on_editor_committed_edits(target, event, cx);
            },
        )
    }

    fn on_editor_committed_edits(
        &mut self,
        target: ViewTarget,
        event: &InputEditsEvent,
        cx: &mut Context<Self>,
    ) {
        if self.session.resolve(target).is_err() {
            return;
        }
        let doc_id = target.document.doc_id;
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) else {
            return;
        };

        let Some(outcome) = ed.ingest_edits_event(event) else {
            return;
        };
        self.finish_committed_mutation(doc_id, outcome, cx);
    }

    pub(super) fn finish_committed_mutation(
        &mut self,
        doc_id: DocumentId,
        outcome: CommittedMutationOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self
            .session
            .views
            .values()
            .find(|view| view.document.doc_id == doc_id)
            .and_then(|view| self.session.target(view.id))
        else {
            return;
        };
        let commit_admissible =
            self.document_commit_admissible(doc_id) && !self.is_lifecycle_busy();
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) else {
            return;
        };
        let doc_id = ed.doc_id;
        let first_baseline = commit_admissible && ed.durable_revision.0 == 0 && ed.is_dirty();
        let immediate_commit = if first_baseline {
            ed.build_recovery_commit()
        } else {
            None
        };

        self.last_edit_instant = Some(Instant::now());
        if self.search.invalidate_for_doc_edit(doc_id, outcome.new_rev) {
            self.rebuild_search_item_sizes();
            self.sync_active_search_decorations(cx);
        }

        if let Some(commit) = immediate_commit
            && let Some(recovery) = &self.recovery
        {
            let rev = commit.new_rev;
            if let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) {
                match recovery.try_commit(commit) {
                    Ok(()) => ed.record_commit_queued(rev),
                    Err(err) if err.kind == ErrorKind::Busy => ed.record_commit_busy(),
                    Err(err) => {
                        self.recovery_degraded_error = Some(err.message);
                    }
                }
            }
        }

        if let Some(PendingSavepointCheck {
            seq,
            checked_text_seq,
            saved_identity,
            ..
        }) = outcome.savepoint_check
        {
            self.spawn_savepoint_verification(
                target,
                seq,
                checked_text_seq,
                saved_identity,
                outcome.post_rope,
                cx,
            );
        }

        cx.notify();
    }

    pub(super) fn spawn_savepoint_verification(
        &self,
        target: ViewTarget,
        seq: u64,
        checked_text_seq: u64,
        saved_identity: ContentIdentity,
        rope: Rope,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |weak, cx| {
            let matches = cx
                .background_executor()
                .spawn(async move { saved_identity.matches_rope(&rope) })
                .await;
            let _ = weak.update(cx, |this, cx| {
                if this.session.resolve(target).is_err() {
                    return;
                }
                if let Some(TabSlot::Editable(ed)) = this.documents.get_mut(&target.document.doc_id)
                {
                    ed.apply_savepoint_verification(seq, checked_text_seq, matches);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Single-flight cooperative background line-index driver across all open `Paged` tabs (`B7`).
    ///
    /// Guarantees at most 1 UI-scheduled `index_step` task runs at any instant, prioritizes the
    /// active tab when idle, yields to foreground `viewport()` loads (`!pv.is_loading`), and
    /// retries `ErrorKind::Busy` asynchronously without dropping or failing sparse indexing.
    pub(crate) fn ensure_paged_indexer_running(&mut self, cx: &mut Context<Self>) {
        if self.paged_indexer_running {
            return;
        }
        let has_incomplete = self.documents.values().any(|slot| {
            matches!(
                slot,
                TabSlot::Paged(pv)
                    if matches!(pv.index_status, IndexStatus::Indexing { .. })
                        && pv.source_error.is_none()
            )
        });
        if !has_incomplete {
            return;
        }
        self.paged_indexer_running = true;

        cx.spawn(async move |weak, cx| {
            loop {
                let next_job = weak
                    .update(cx, |this, _cx| {
                        if this.shutting_down || this.ordered_view_ids().is_empty() {
                            this.paged_indexer_running = false;
                            return None;
                        }
                        let len = this.ordered_view_ids().len();
                        let active_position = this.active_view_id().and_then(|id| this.ordered_view_ids().iter().position(|v| *v == id)).unwrap_or(0);
                        let pick = (0..len).map(|offset| if offset == 0 { active_position } else { (this.paged_index_rr_cursor + offset) % len })
                            .find(|&position| matches!(this.view_slot(this.ordered_view_ids()[position]), Some(TabSlot::Paged(pv)) if !pv.is_loading && pv.source_error.is_none() && matches!(pv.index_status, IndexStatus::Indexing { .. })));
                        if let Some(position) = pick {
                            this.paged_index_rr_cursor = (position + 1) % len;
                            let target = this.view_target(this.ordered_view_ids()[position])?;
                            if let Some(TabSlot::Paged(pv)) = this.view_slot(target.view_id) {
                                return Some(Ok((target, pv.handle.clone(), pv.source_gen, pv.index_cancel_token())));
                            }
                        }

                        // Check if any tab still needs indexing once its foreground viewport finishes.
                        let any_pending_after_viewport = this.documents.values().any(|slot| {
                            matches!(
                                slot,
                                TabSlot::Paged(pv)
                                    if pv.source_error.is_none()
                                        && matches!(pv.index_status, IndexStatus::Indexing { .. })
                            )
                        });
                        if any_pending_after_viewport {
                            Some(Err(()))
                        } else {
                            this.paged_indexer_running = false;
                            None
                        }
                    })
                    .ok()
                    .flatten();

                match next_job {
                    None => break,
                    Some(Err(())) => {
                        // Active paged tab is loading a foreground viewport; yield briefly.
                        cx.background_executor()
                            .timer(Duration::from_millis(25))
                            .await;
                    }
                    Some(Ok((target, handle, source_gen, cancel))) => {
                        if cancel.is_cancelled() {
                            continue;
                        }
                        let step = cx
                            .background_executor()
                            .spawn(async move { handle.index_step(source_gen, &cancel) })
                            .await;

                        let mut backoff_ms = 10u64;
                        let alive = weak
                            .update(cx, |this, cx| {
                                if this.session.resolve(target).is_err() { return !this.shutting_down; }
                                if let Some(TabSlot::Paged(pv)) =
                                    this.documents.get_mut(&target.document.doc_id)
                                {
                                    if pv.source_gen != source_gen { return !this.shutting_down; }
                                    match step {
                                        Ok(status) => {
                                            pv.index_status = status;
                                            cx.notify();
                                        }
                                        Err(err) if err.kind == ErrorKind::Busy => {
                                            backoff_ms = 50;
                                        }
                                        Err(err) if err.kind == ErrorKind::Cancelled => {}
                                        Err(err) => {
                                            pv.source_error = Some(err.message);
                                            if this.search.target_view == Some(target) {
                                                this.search.cancel_active();
                                                this.search.status = SearchStatus::Invalidated;
                                            }
                                            cx.notify();
                                        }
                                    }
                                }
                                !this.shutting_down
                            })
                            .unwrap_or(false);
                        if !alive {
                            break;
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(backoff_ms))
                            .await;
                    }
                }
            }
        })
        .detach();
    }

    /// Executes the clean shutdown flush barrier (`§8.2`, `§8.5`, `R01`, `R05`).
    pub fn initiate_clean_quit(
        &mut self,
        force_without_recovery: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.prepare_quit_barrier(force_without_recovery, window, cx) {
            return;
        }
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.search.cancel_active();

        let has_any_dirty = self.documents.values().any(TabSlot::close_guard);
        let has_oversized_dirty = self.documents.values().any(|slot| match slot {
            TabSlot::Editable(ed) => {
                ed.is_dirty() && ed.committed_byte_len() > MAX_ORDINARY_DOC_BYTES as u64
            }
            _ => false,
        });
        if has_oversized_dirty && !force_without_recovery {
            if self.smoke_outcome.is_some() {
                self.abort_smoke_quit(
                    "oversized dirty tab (> 64 MiB) blocked automatic recovery during smoke quit",
                );
            }
            self.shutting_down = false;
            self.open_degraded_quit_dialog(
                "An open modified tab exceeds the 64 MiB automatic recovery limit. Save it to disk first, or force quit without saving?",
                window,
                cx,
            );
            return;
        }

        let maybe_shell = if self.smoke_outcome.is_some() {
            self.shell_store.take()
        } else {
            self.shell_store.clone()
        };
        let pending_shell_snapshot = self.shell_pending_queue.then(|| self.shell_state.clone());

        // Guard 2: In smoke mode, `.take()` drops EditorWorkspace's clone of `RecoveryHandle`
        // so that once `recovery.shutdown()` ACKs on the worker, `drop(recovery)` releases the
        // sole handle before sequential post-shutdown SQLite verification opens a fresh handle.
        let maybe_recovery = if self.smoke_outcome.is_some() {
            self.recovery.take()
        } else {
            self.recovery.clone()
        };
        let Some(recovery) = maybe_recovery else {
            if self.smoke_outcome.is_some() {
                self.abort_smoke_quit("RecoveryHandle unavailable during smoke shutdown");
            }
            if has_any_dirty && !force_without_recovery {
                self.shutting_down = false;
                self.open_degraded_quit_dialog(
                    "Recovery database is offline; quitting now would lose unsaved drafts.",
                    window,
                    cx,
                );
                return;
            }
            if let Some(shell) = maybe_shell {
                cx.spawn_in(window, async move |weak, cx| {
                    let shell_res = cx
                        .background_executor()
                        .spawn(async move {
                            flush_and_shutdown_shell(&shell, pending_shell_snapshot, true)
                        })
                        .await;
                    if let Err(err) = shell_res {
                        eprintln!("editor preferences shutdown warning: {}", err.message);
                    }
                    let _ = weak.update_in(cx, |this, _, cx| {
                        this.quit_confirmed = true;
                        cx.quit();
                    });
                })
                .detach();
                return;
            }
            self.quit_confirmed = true;
            cx.quit();
            return;
        };
        // Layout and lifecycle acknowledgments are complete before this final text barrier.
        let mut final_commits = Vec::new();
        let expected_layout = self.session.clone();
        let required_revisions = self
            .documents
            .values()
            .filter_map(|slot| match slot {
                TabSlot::Editable(ed)
                    if ed.committed_byte_len() <= MAX_ORDINARY_DOC_BYTES as u64
                        && (ed.has_checkpoint || ed.visible_revision > Revision(0)) =>
                {
                    Some((slot.document_key(), ed.visible_revision))
                }
                TabSlot::Dehydrated(stub)
                | TabSlot::Hydrating { stub, .. }
                | TabSlot::OverBudgetDraft { stub, .. }
                | TabSlot::Failed { stub, .. }
                    if stub.has_checkpoint =>
                {
                    Some((stub.key(), stub.durable_rev))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut original_readonly = Vec::new();
        self.status_flash = Some((
            "Waiting for durable shutdown; live drafts remain frozen".into(),
            Instant::now(),
        ));
        cx.notify();
        for slot in self.documents.values_mut() {
            if let TabSlot::Editable(ed) = slot {
                original_readonly
                    .push((ed.doc_id, ed.editor().read(cx).presentation().is_readonly()));
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
                if let Some(commit) = ed.build_recovery_commit() {
                    final_commits.push(commit);
                }
            }
        }

        let smoke_outcome = self.smoke_outcome.clone();
        let data_dir = self.cli.data_dir.clone();
        let smoke_report_path = self.cli.smoke_report.clone();
        let process_start = self.process_start;

        cx.spawn_in(window, async move |weak, cx| {
            let (barrier_res, shell_res, finalized_smoke_report, shutdown_unknown, actual_shutdown_ack, shutdown_disconnected) = cx
                .background_executor()
                .spawn(async move {
                    let mut shutdown_unknown=false;
                    let mut actual_shutdown_ack=false;
                    let mut shutdown_disconnected=false;
                    let primary_res: Result<(), ServiceError> = (|| {
                        for mut commit in final_commits {
                            // Retry briefly if mailbox is busy so final quit never drops committed edits.
                            let deadline = Instant::now() + Duration::from_secs(5);
                            loop {
                                match recovery.try_commit(commit.clone()) {
                                    Ok(()) => break,
                                    Err(e)
                                        if e.kind == ErrorKind::Busy
                                            && Instant::now() < deadline =>
                                    {
                                        commit.deltas.clear();
                                        std::thread::sleep(Duration::from_millis(20));
                                    }
                                    Err(e) if e.kind == ErrorKind::Stale => break,
                                    Err(e) => return Err(e),
                                }
                            }
                        }
                        let shutdown_deadline = Instant::now() + Duration::from_secs(5);
                        let rx = loop {
                            match recovery.shutdown() {
                                Ok(rx) => break rx,
                                Err(e)
                                    if e.kind == ErrorKind::Busy
                                        && Instant::now() < shutdown_deadline =>
                                {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                Err(e) => return Err(e),
                            }
                        };
                        match rx.recv(){
                            Ok(result)=>{actual_shutdown_ack=result.is_ok();result}
                            Err(error)=>{
                                shutdown_disconnected=true;
                                match editor_services::inspect_recovery_shutdown(data_dir.join("recovery.sqlite3"),&expected_layout,&required_revisions){
                                    Ok(true)=>Ok(()),
                                    Ok(false)=>Err(ServiceError::new(ErrorKind::Storage,"Shutdown did not durably cover every frozen revision; read-only inspection confirms unchanged document identities and retains the live buffers")),
                                    Err(inspection)=>{shutdown_unknown=true;Err(ServiceError::new(ErrorKind::Storage,format!("Shutdown outcome remains indeterminate ({error}); read-only reconciliation: {inspection}. Frozen buffers retained; Export Copy remains available.")))}
                                }
                            },
                        }
                    })();

                    // Guard 2: Drop primary `RecoveryHandle` BEFORE opening any verification DB handle.
                    drop(recovery);

                    // Flush/shutdown UI shell preferences worker separately so preference I/O
                    // errors never masquerade as draft-recovery failures or block rescue save!
                    let should_shutdown_shell =
                        primary_res.is_ok() || (!shutdown_unknown&&(!has_any_dirty || force_without_recovery));
                    let shell_res: Result<(), ServiceError> = if let Some(shell) = maybe_shell {
                        let res = flush_and_shutdown_shell(
                            &shell,
                            pending_shell_snapshot,
                            should_shutdown_shell,
                        );
                        drop(shell);
                        res
                    } else {
                        Ok(())
                    };

                    let smoke_report = if let Some(slot) = smoke_outcome.as_ref() {
                        let mut report = slot.take().unwrap_or_default();
                        if let Err(err) = &shell_res {
                            report.shell_persistence_warning = Some(err.message.clone());
                        }
                        match &primary_res {
                            Ok(()) => {
                                report.shutdown_durable_ack_observed = actual_shutdown_ack;
                                if report.in_app_checks_passed
                                    && let (Some(fresh_id), Some(old_id)) = (
                                        report.expected_post_shutdown_fresh_doc,
                                        report.tombstoned_old_doc,
                                    )
                                {
                                    match crate::smoke::verify_post_shutdown_manifest(
                                        &data_dir,
                                        fresh_id,
                                        old_id,
                                        report.expected_clean_untitled_doc,
                                    ) {
                                        Ok(()) => {
                                            report.post_shutdown_manifest_verified = true;
                                            if report.expected_clean_untitled_doc.is_some() {
                                                report.clean_untitled_restart_verified = true;
                                            }
                                        }
                                        Err(err) => {
                                            report.failure_reason = Some(err);
                                        }
                                    }
                                }
                                if report.in_app_checks_passed
                                    && report.failure_reason.is_none()
                                    && report.scenario == "usability"
                                {
                                    match crate::smoke::verify_post_shutdown_usability(
                                        &data_dir, &report,
                                    ) {
                                        Ok(()) => {
                                            report.post_shutdown_manifest_verified = true;
                                        }
                                        Err(err) => {
                                            report.failure_reason = Some(err);
                                        }
                                    }
                                }
                                if report.in_app_checks_passed && report.failure_reason.is_none() && report.expected_session_layout.is_some() {
                                    match crate::smoke::verify_post_shutdown_layout(&data_dir,&report) {
                                        Ok(()) => {
                                            if report.scenario=="groups" { report.group_restart_layout_verified=true; }
                                            if report.scenario=="startup" { report.startup_restart_layout_verified=true; }
                                        }
                                        Err(error) => report.failure_reason=Some(error),
                                    }
                                }
                                report.total_elapsed_ms =
                                    process_start.elapsed().as_secs_f64() * 1000.0;
                                report.passed = report.in_app_checks_passed
                                    && report.shutdown_durable_ack_observed
                                    && report.failure_reason.is_none();
                            }
                            Err(err) => {
                                report.shutdown_durable_ack_observed = false;
                                report.passed = false;
                                report.total_elapsed_ms =
                                    process_start.elapsed().as_secs_f64() * 1000.0;
                                report.failure_reason = Some(match report.failure_reason.take() {
                                    Some(prev) => {
                                        format!("{prev}; shutdown error: {}", err.message)
                                    }
                                    None => format!("shutdown error: {}", err.message),
                                });
                            }
                        }
                        Some(report)
                    } else {
                        None
                    };

                    (primary_res, shell_res, smoke_report, shutdown_unknown, actual_shutdown_ack, shutdown_disconnected)
                })
                .await;

            if let Some(report) = finalized_smoke_report {
                // Guard 3: Emit report + flush stdout/stderr BEFORE `[NSApp terminate:nil]` (`cx.quit`)
                // and exit `1` on smoke failure so macOS AppKit never swallows failures or buffers.
                crate::smoke::emit_smoke_report(&report, smoke_report_path.as_deref());
                if report.passed {
                    if weak
                        .update_in(cx, |this, _, cx| {
                            this.shutdown_durable_ack.store(actual_shutdown_ack, Ordering::Release);
                            this.quit_confirmed = true;
                            cx.quit();
                        })
                        .is_err()
                    {
                        std::process::exit(0);
                    }
                } else {
                    std::process::exit(1);
                }
                return;
            }

            if let Err(shell_err) = &shell_res {
                eprintln!(
                    "editor preferences flush warning during quit (draft recovery unaffected): {}",
                    shell_err.message
                );
            }

            let _ = weak.update_in(cx, |this, window, cx| match barrier_res {
                Ok(()) => {
                    this.shutdown_durable_ack.store(actual_shutdown_ack, Ordering::Release);
                    this.quit_confirmed = true;
                    cx.quit();
                }
                Err(err) if shutdown_unknown=>{
                    this.recovery_degraded_error=Some(err.message.clone());
                    this.notify_error(err,window,cx);
                    cx.notify();
                }
                Err(err) if has_any_dirty && !force_without_recovery => {
                    this.shutting_down = false;
                    if shutdown_disconnected{this.adopt_read_only_recovery();}
                    if let Err(shell_err) = shell_res {
                        this.shell_persistence_warning = Some(shell_err.message);
                    }
                    this.restore_readonly(original_readonly,cx);
                    this.open_degraded_quit_dialog(&err.message, window, cx);
                    cx.notify();
                }
                Err(_) => {
                    this.quit_confirmed = true;
                    cx.quit();
                }
            });
        })
        .detach();
    }

    fn abort_smoke_quit(&self, reason: &str) -> ! {
        let mut report = self
            .smoke_outcome
            .as_ref()
            .and_then(|s| s.take())
            .unwrap_or_default();
        report.shutdown_durable_ack_observed = false;
        report.passed = false;
        report.total_elapsed_ms = self.process_start.elapsed().as_secs_f64() * 1000.0;
        report.failure_reason = Some(match report.failure_reason.take() {
            Some(prev) => format!("{prev}; {reason}"),
            None => reason.to_string(),
        });
        crate::smoke::emit_smoke_report(&report, self.cli.smoke_report.as_deref());
        std::process::exit(1);
    }

    fn open_degraded_quit_dialog(&self, reason: &str, window: &mut Window, cx: &mut Context<Self>) {
        let entity = cx.entity();
        let msg = format!(
            "Could not persist unsaved drafts before quitting ({reason}). Save your modified tabs to disk first, or force quit without draft recovery?"
        );
        window.open_dialog(cx, move |dialog, _, _| {
            let e_force = entity.clone();
            dialog
                .title("Draft Recovery Flush Failed")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Quit Without Saving")
                        .ok_variant(ButtonVariant::Danger)
                        .show_cancel(true)
                        .cancel_text("Stay Open & Save")
                        .on_ok(move |_, window, cx| {
                            e_force.update(cx, |this, cx| {
                                this.initiate_clean_quit(true, window, cx);
                            });
                            true
                        }),
                )
                .footer(
                    gpui_kit::component::h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            gpui_kit::component::dialog::DialogClose::new()
                                .trigger(|button| button.label("Stay Open & Save")),
                        )
                        .child(
                            gpui_kit::component::dialog::DialogAction::new().child(
                                gpui_kit::component::button::Button::new("quit-without-recovery")
                                    .danger()
                                    .label("Quit Without Saving"),
                            ),
                        ),
                )
                .child(gpui_kit::div().text_sm().child(msg.clone()))
        });
    }

    // --- Search, Replace & Jump Actions ---

    pub fn open_search_bar(
        &mut self,
        with_replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_or_dismiss_group_ui(window, cx);
        self.search.visible = true;
        self.search.replace_visible = with_replace;
        self.goto_visible = false;
        self.palette_visible = false;
        self.palette_prior_origin = None;

        // Seed search box from active selection if non-empty and short.
        if let Some(TabSlot::Editable(ed)) = self.active_slot() {
            let sel = ed.editor().read(cx).selected_range();
            if sel.end > sel.start && sel.end - sel.start <= 256 {
                let selected = ed.committed_rope().slice(sel).to_string();
                if !selected.contains('\n') {
                    self.search_query_input
                        .update(cx, |input, cx| input.set_value(selected, window, cx));
                }
            }
        }

        self.search_query_input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.run_search_from_inputs(window, cx);
        cx.notify();
    }

    pub fn close_overlays(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel_or_dismiss_group_ui(window, cx) {
            return;
        }
        if self.cancel_pending_lifecycle(window, cx) {
            return;
        }
        self.search.visible = false;
        self.search.cancel_active();
        self.goto_visible = false;
        self.palette_visible = false;
        self.palette_prior_origin = None;
        if let Some(TabSlot::Editable(ed)) = self.active_slot() {
            ed.clear_search_highlights(cx);
            ed.focus(window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    pub fn run_search_from_inputs(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(view_target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        let query = self.search_query_input.read(cx).value().to_string();
        let Some(slot) = self.active_slot() else {
            return;
        };

        if query.is_empty() {
            self.search.clear_results();
            self.rebuild_search_item_sizes();
            self.sync_active_search_decorations(cx);
            cx.notify();
            return;
        }

        match slot {
            TabSlot::Editable(ed) => {
                let doc_id = ed.doc_id;
                let rev = ed.visible_revision;
                let rope = ed.committed_rope().clone();

                match self.search.query_kind {
                    SearchQueryKind::LiteralStreaming => {
                        self.search.start_streaming_literal(
                            &self.search_service,
                            view_target,
                            &query,
                            SearchTarget::EditableSnapshot { doc_id, rev, rope },
                        );
                        self.rebuild_search_item_sizes();
                    }
                    SearchQueryKind::CaseInsensitive | SearchQueryKind::Regex => {
                        let kind = self.search.query_kind;
                        let (qid, cancel) =
                            self.search
                                .begin_background_extended_search(view_target, rev, &query);
                        self.rebuild_search_item_sizes();
                        self.sync_active_search_decorations(cx);
                        let query_owned = query.clone();
                        cx.spawn(async move |weak, cx| {
                            let res = cx
                                .background_executor()
                                .spawn(async move {
                                    run_editable_extended_search(&rope, &query_owned, kind, &cancel)
                                })
                                .await;
                            let _ = weak.update(cx, |this, cx| {
                                if this.active_view_id() != Some(view_target.view_id)
                                    || this.session.resolve(view_target).is_err()
                                    || this.search.target_view != Some(view_target)
                                {
                                    return;
                                }
                                let current_doc_rev =
                                    this.documents.values().find_map(|s| match s {
                                        TabSlot::Editable(ed) if ed.doc_id == doc_id => {
                                            Some(ed.visible_revision)
                                        }
                                        _ => None,
                                    });
                                if this.search.apply_extended_search_result(
                                    qid,
                                    current_doc_rev,
                                    res,
                                ) {
                                    this.rebuild_search_item_sizes();
                                    this.sync_active_search_decorations(cx);
                                    cx.notify();
                                }
                            });
                        })
                        .detach();
                    }
                }
            }
            TabSlot::Paged(pv) => {
                // S06: Explicitly refuse unsupported large-file regex / case-folding!
                if self.search.query_kind != SearchQueryKind::LiteralStreaming {
                    self.search.cancel_active();
                    self.search.matches.clear();
                    self.search.selected_index = None;
                    self.search.status = SearchStatus::Error(
                        "Paged Large-File mode supports bounded UTF-8 Literal streaming search only (Regex/Case-Insensitive disabled per §7.3)."
                            .to_string(),
                    );
                    self.rebuild_search_item_sizes();
                    cx.notify();
                    return;
                }
                let target = SearchTarget::PagedFile {
                    handle: pv.handle.clone(),
                    source_gen: pv.source_gen,
                    encoding: pv.meta.encoding,
                };
                self.search.start_streaming_literal(
                    &self.search_service,
                    view_target,
                    &query,
                    target,
                );
                self.rebuild_search_item_sizes();
            }
            _ => {
                self.search.clear_results();
                self.rebuild_search_item_sizes();
            }
        }
        cx.notify();
    }

    pub(crate) fn sync_active_search_decorations(&mut self, cx: &mut Context<Self>) {
        let fill = cx.theme().warning.opacity(0.24);
        let active = cx.theme().primary.opacity(0.42);
        let selected_ix = self.search.selected_index;

        match self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        {
            Some(TabSlot::Editable(ed)) => {
                if !self.search.visible || self.search.target_doc_id != Some(ed.doc_id) {
                    ed.clear_search_highlights(cx);
                    return;
                }
                let ranges: Vec<std::ops::Range<usize>> = self
                    .search
                    .matches
                    .iter()
                    .take(1024)
                    .filter_map(|m| {
                        Some(
                            usize::try_from(m.byte_range.start).ok()?
                                ..usize::try_from(m.byte_range.end).ok()?,
                        )
                    })
                    .collect();
                ed.set_search_highlights(&ranges, selected_ix, fill, active, cx);
            }
            Some(TabSlot::Paged(pv)) => {
                pv.highlighted_byte_range = selected_ix
                    .and_then(|ix| self.search.matches.get(ix))
                    .map(|m| m.byte_range.clone());
            }
            _ => {}
        }
    }

    pub fn jump_to_search_match(
        &mut self,
        match_index: usize,
        focus_editor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy()
            || self.search.target_view != self.active_view_id().and_then(|id| self.view_target(id))
        {
            return;
        }
        let Some(hit) = self.search.matches.get(match_index).cloned() else {
            return;
        };
        self.search.selected_index = Some(match_index);
        self.sync_active_search_decorations(cx);

        match self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        {
            Some(TabSlot::Editable(ed)) => {
                if let (Ok(s), Ok(e)) = (
                    usize::try_from(hit.byte_range.start),
                    usize::try_from(hit.byte_range.end),
                ) {
                    if focus_editor {
                        ed.jump_to_byte_range(s..e, window, cx);
                    } else {
                        ed.editor().update(cx, |state, cx| {
                            state.set_selected_range(s..e, cx);
                        });
                    }
                }
            }
            Some(TabSlot::Paged(pv)) => {
                pv.highlighted_byte_range = Some(hit.byte_range.clone());
                let target_offset = hit.byte_range.start.saturating_sub(512);
                if let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) {
                    self.request_paged_viewport(
                        target,
                        ViewportAnchor::ByteOffset(target_offset),
                        window,
                        cx,
                    );
                };
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn replace_current_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_replace(true, window, cx);
    }

    pub fn replace_all_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.execute_replace(false, window, cx);
    }

    fn execute_replace(&mut self, single: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mutation_allowed() {
            return;
        }
        let Some(view_target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        let query = self.search_query_input.read(cx).value().to_string();
        let replacement = self.search_replace_input.read(cx).value().to_string();

        let Some(TabSlot::Editable(ed)) = self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        else {
            window.push_notification(
                Notification::info("Replace is available on Editable documents only (Paged Large-File mode is read-only in v1)."),
                cx,
            );
            return;
        };

        if ed.is_readonly() {
            window.push_notification(
                Notification::warning("Cannot replace text in a read-only document."),
                cx,
            );
            return;
        }

        let doc_id = ed.doc_id;
        let current_rev = ed.visible_revision;
        let matches_fresh = !self.search.is_running()
            && self.search.target_view == Some(view_target)
            && self.search.target_doc_id == Some(doc_id)
            && self.search.target_editable_rev == Some(current_rev)
            && self.search.last_query_text == query;

        if matches_fresh {
            self.apply_planned_replace_on_active(
                view_target,
                current_rev,
                &query,
                &replacement,
                single,
                window,
                cx,
            );
            return;
        }

        // Compute fresh snapshot matches off the UI thread with revision & budget guards (`U-B3`).
        let rope = ed.committed_rope().clone();
        let kind = self.search.query_kind;
        let (qid, cancel) =
            self.search
                .begin_background_extended_search(view_target, current_rev, &query);
        self.rebuild_search_item_sizes();
        cx.notify();

        cx.spawn_in(window, async move |weak, cx| {
            let q_worker = query.clone();
            let res = cx
                .background_executor()
                .spawn(async move { run_editable_extended_search(&rope, &q_worker, kind, &cancel) })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                if !this.mutation_allowed() || this.active_view_id() != Some(view_target.view_id) || this.session.resolve(view_target).is_err() || this.search.target_view != Some(view_target) { return; }
                let live_rev = this.documents.values().find_map(|s| match s {
                    TabSlot::Editable(ed) if ed.doc_id == doc_id => Some(ed.visible_revision),
                    _ => None,
                });
                if !this.search.apply_extended_search_result(qid, live_rev, res) {
                    return;
                }
                this.rebuild_search_item_sizes();
                this.sync_active_search_decorations(cx);

                if let SearchStatus::Error(msg) = &this.search.status {
                    window.push_notification(Notification::error(msg.clone()), cx);
                    cx.notify();
                    return;
                }
                if live_rev != Some(current_rev) {
                    window.push_notification(
                        Notification::warning(
                            "Document changed while computing replacements; review updated matches and retry.",
                        ),
                        cx,
                    );
                    cx.notify();
                    return;
                }
                this.apply_planned_replace_on_active(
                    view_target,
                    current_rev,
                    &query,
                    &replacement,
                    single,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_planned_replace_on_active(
        &mut self,
        view_target: ViewTarget,
        base_rev: Revision,
        query: &str,
        replacement: &str,
        single: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed()
            || self.active_view_id() != Some(view_target.view_id)
            || self.session.resolve(view_target).is_err()
        {
            return;
        }
        let Some(TabSlot::Editable(ed)) = self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        else {
            return;
        };
        if ed.doc_id != view_target.document.doc_id {
            return;
        }
        let single_ix = if single {
            Some(self.search.selected_index.unwrap_or(0))
        } else {
            None
        };
        let edits = match plan_replacement_edits(
            ed.committed_rope(),
            query,
            replacement,
            self.search.query_kind,
            &self.search.matches,
            single_ix,
        ) {
            Ok(edits) => edits,
            Err(msg) => {
                window.push_notification(Notification::error(msg), cx);
                return;
            }
        };
        if edits.is_empty() {
            return;
        }
        let count = edits.len();
        match self.apply_active_atomic_edits(base_rev, &edits, window, cx) {
            Ok(_) => {
                self.status_flash = Some((
                    format!(
                        "Replaced {count} match{}",
                        if count == 1 { "" } else { "es" }
                    ),
                    Instant::now(),
                ));
                self.run_search_from_inputs(window, cx);
            }
            Err(err) => {
                window.push_notification(
                    Notification::error(format!("Atomic replace rejected: {err}")),
                    cx,
                );
            }
        }
    }

    pub fn execute_goto_from_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mutation_allowed() {
            return;
        }
        let raw = self.goto_input.read(cx).value().to_string();
        let is_paged = matches!(self.active_slot(), Some(TabSlot::Paged(_)));

        match parse_jump_query(&raw, is_paged) {
            Ok(anchor) => {
                self.goto_visible = false;
                match self.active_slot() {
                    Some(TabSlot::Editable(ed)) => match anchor {
                        ViewportAnchor::ByteOffset(b) => {
                            let off = usize::try_from(b).unwrap_or(usize::MAX);
                            ed.jump_to_byte_range(off..off, window, cx);
                        }
                        ViewportAnchor::ApproxLine(line_0) => {
                            let line_1 =
                                usize::try_from(line_0.saturating_add(1)).unwrap_or(usize::MAX);
                            ed.jump_to_line_1based(line_1, window, cx);
                        }
                    },
                    Some(TabSlot::Paged(_)) => {
                        if let Some(target) =
                            self.active_view_id().and_then(|id| self.view_target(id))
                        {
                            self.request_paged_viewport(target, anchor, window, cx);
                        };
                        self.focus_handle.focus(window, cx);
                    }
                    _ => {}
                }
                cx.notify();
            }
            Err(msg) => {
                window.push_notification(Notification::warning(msg), cx);
            }
        }
    }

    // --- Action Listeners ---

    pub(crate) fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.create_untitled_tab(true, window, cx);
    }

    pub(crate) fn on_open_file(
        &mut self,
        _: &OpenFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |weak, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let _ = weak.update_in(cx, |this, window, cx| {
                    this.open_paths(paths, window, cx);
                });
            }
        })
        .detach();
    }

    pub(crate) fn on_save_file(
        &mut self,
        _: &SaveFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) {
            self.save_view(target, None, false, false, window, cx);
        };
    }

    pub(crate) fn on_save_file_as(
        &mut self,
        _: &SaveFileAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) {
            self.prompt_save_view_as(target, false, window, cx);
        };
    }

    pub(crate) fn on_close_active_tab(
        &mut self,
        _: &CloseActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) {
            self.request_close_view(target, window, cx);
        };
    }

    pub(crate) fn on_reopen_closed_tab(
        &mut self,
        _: &ReopenClosedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reopen_recently_closed(window, cx);
    }

    pub(crate) fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.ordered_view_ids();
        if let Some(position) = self
            .active_view_id()
            .and_then(|id| ids.iter().position(|v| *v == id))
        {
            if let Some(target) = self.view_target(ids[(position + 1) % ids.len()]) {
                self.activate_view(target, window, cx);
            }
        }
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let ids = self.ordered_view_ids();
        if let Some(position) = self
            .active_view_id()
            .and_then(|id| ids.iter().position(|v| *v == id))
        {
            if let Some(target) = self.view_target(ids[(position + ids.len() - 1) % ids.len()]) {
                self.activate_view(target, window, cx);
            }
        }
    }

    pub(crate) fn on_toggle_search(
        &mut self,
        _: &ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_bar(false, window, cx);
    }

    pub(crate) fn on_kit_search(
        &mut self,
        _: &gpui_kit::component::input::Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_bar(false, window, cx);
    }

    pub(crate) fn on_toggle_replace(
        &mut self,
        _: &ToggleReplace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_bar(true, window, cx);
    }

    pub(crate) fn on_kit_replace(
        &mut self,
        _: &gpui_kit::component::input::Replace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_bar(true, window, cx);
    }

    pub(crate) fn on_find_next(
        &mut self,
        _: &FindNextMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.search.select_next();
        if let Some(ix) = self.search.selected_index {
            self.jump_to_search_match(ix, false, window, cx);
        }
    }

    pub(crate) fn on_find_prev(
        &mut self,
        _: &FindPrevMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.search.select_prev();
        if let Some(ix) = self.search.selected_index {
            self.jump_to_search_match(ix, false, window, cx);
        }
    }

    pub(crate) fn on_cancel_search(
        &mut self,
        _: &CancelActiveSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search.is_running() {
            self.search.cancel_active();
            cx.notify();
        } else {
            self.close_overlays(window, cx);
        }
    }

    pub(crate) fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.cancel_or_dismiss_group_ui(window, cx) {
            return;
        }
        if self.cancel_pending_lifecycle(window, cx) {
            return;
        }
        if self.palette_visible {
            self.dismiss_palette(window, cx);
        } else if self.search.visible || self.goto_visible {
            self.close_overlays(window, cx);
        }
    }

    pub(crate) fn on_open_goto(
        &mut self,
        _: &OpenGoToOffset,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_or_dismiss_group_ui(window, cx);
        self.palette_visible = false;
        self.palette_prior_origin = None;
        self.goto_visible = !self.goto_visible;
        if self.goto_visible {
            self.goto_input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        } else if let Some(TabSlot::Editable(ed)) = self.active_slot() {
            ed.focus(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_toggle_theme(
        &mut self,
        _: &ToggleThemeMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (next_mode, next_shell) = if cx.theme().is_dark() {
            (ThemeMode::Light, ShellTheme::Light)
        } else {
            (ThemeMode::Dark, ShellTheme::Dark)
        };
        Theme::change(next_mode, Some(window), cx);
        if self.shell_state.theme_mode != Some(next_shell) {
            self.shell_state.theme_mode = Some(next_shell);
            if !self.shell_loaded {
                self.shell_early_overrides.theme_mode = true;
            }
            self.queue_shell_persistence(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_toggle_line_ending(
        &mut self,
        _: &ToggleLineEnding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Editable(ed)) = self.active_slot() else {
            return;
        };
        let next = match ed.meta.line_ending {
            LineEnding::Lf => LineEnding::Crlf,
            LineEnding::Crlf => LineEnding::Lf,
            LineEnding::MixedReadOnly => {
                window.push_notification(
                    Notification::warning(
                        "Mixed line endings opened read-only to preserve fidelity.",
                    ),
                    cx,
                );
                return;
            }
        };
        self.set_active_line_ending(next, window, cx);
    }

    pub(crate) fn on_toggle_bom(
        &mut self,
        _: &ToggleUtf8Bom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Editable(ed)) = self.active_slot() else {
            return;
        };
        let next = match ed.meta.encoding {
            EncodingSpec::Utf8 => EncodingSpec::Utf8Bom,
            EncodingSpec::Utf8Bom => EncodingSpec::Utf8,
            EncodingSpec::UnsupportedReadOnly => {
                window.push_notification(
                    Notification::warning("Unsupported encoding is read-only."),
                    cx,
                );
                return;
            }
        };
        self.set_active_encoding(next, window, cx);
    }

    pub(crate) fn on_cycle_language(
        &mut self,
        _: &CycleLanguage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Editable(ed)) = self.active_slot() else {
            return;
        };
        let next = next_language(ed.language.id);
        self.set_active_language_override(Some(next.id), window, cx);
    }
    pub(crate) fn on_cycle_search_kernel(
        &mut self,
        _: &CycleSearchKernel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let kernel = self.search.cycle_algorithm();
        self.status_flash = Some((format!("Search kernel: {kernel:?}"), Instant::now()));
        if self.search.visible {
            self.run_search_from_inputs(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn on_reload_active(
        &mut self,
        _: &ReloadActiveDocument,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) {
            self.request_reload_view(target, window, cx);
        };
    }

    pub(crate) fn on_external_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_paths(paths.paths().to_vec(), window, cx);
    }

    pub(crate) fn on_quit(&mut self, _: &QuitEditor, window: &mut Window, cx: &mut Context<Self>) {
        self.initiate_clean_quit(false, window, cx);
    }

    pub(crate) fn copy_paged_slice(&mut self, cx: &mut Context<Self>) {
        if let Some(TabSlot::Paged(pv)) = self.active_slot()
            && let Some(slice) = &pv.current_slice
        {
            cx.write_to_clipboard(ClipboardItem::new_string(slice.text.to_string()));
            self.status_flash = Some((
                format!(
                    "Copied {} slice to clipboard",
                    format_bytes(slice.text.len() as u64)
                ),
                Instant::now(),
            ));
            cx.notify();
        }
    }

    pub fn apply_active_atomic_edits(
        &mut self,
        expected_base_rev: Revision,
        edits: &[(std::ops::Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Revision, ApplyEditsError> {
        let target = self
            .active_view_id()
            .and_then(|id| self.view_target(id))
            .ok_or(ApplyEditsError::ReadOnly)?;
        self.apply_view_atomic_edits(target, expected_base_rev, edits, window, cx)
    }

    // --- Daily-Use Shell, Preferences, Format, Language, Palette & Line Operations ---

    #[inline]
    pub fn current_view_prefs(&self) -> EditorViewPrefs {
        EditorViewPrefs::from_shell(&self.shell_state)
    }

    #[inline]
    pub fn sidebar_visible(&self) -> bool {
        self.shell_state.sidebar_visible
    }

    #[inline]
    pub fn recent_files(&self) -> &[PathBuf] {
        &self.shell_state.recent_files
    }

    #[inline]
    pub fn shell_persistence_warning(&self) -> Option<&str> {
        self.shell_persistence_warning.as_deref()
    }

    pub(crate) fn apply_loaded_shell_bootstrap(
        &mut self,
        boot: Result<(UiShellStore, UiShellLoad), ServiceError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.shell_loaded = true;
        match boot {
            Ok((store, load)) => {
                if let Some(warning) = load.warning {
                    self.record_shell_warning(warning, window, cx);
                }
                let merged = merge_loaded_shell_state(
                    &self.shell_state,
                    &load.state,
                    self.shell_early_overrides,
                    &self.shell_early_recent,
                    self.cli.theme,
                );
                self.shell_state = merged.state;
                self.shell_early_recent.clear();
                self.shell_store = Some(store);

                if let Some(theme) = merged.effective_window_theme {
                    let mode = match theme {
                        ShellTheme::Light => ThemeMode::Light,
                        ShellTheme::Dark => ThemeMode::Dark,
                    };
                    Theme::change(mode, Some(window), cx);
                }
                self.apply_prefs_to_active_tab(window, cx);
                if merged.needs_immediate_persist {
                    self.queue_shell_persistence(window, cx);
                }
            }
            Err(err) => {
                self.record_shell_warning(
                    format!(
                        "UI preferences unavailable ({}): session preferences remain in memory only.",
                        err.message
                    ),
                    window,
                    cx,
                );
            }
        }
        cx.notify();
    }

    pub(crate) fn record_shell_warning(
        &mut self,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell_persistence_warning.as_deref() != Some(message.as_str()) {
            self.shell_persistence_warning = Some(message.clone());
            window.push_notification(Notification::warning(message), cx);
        }
    }

    pub(crate) fn queue_shell_persistence(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shutting_down {
            return;
        }
        let Some(store) = self.shell_store.clone() else {
            return;
        };
        match store.queue(self.shell_state.clone()) {
            Ok(()) => {
                self.shell_pending_queue = false;
                self.ensure_shell_flush_waiter(window, cx);
            }
            Err(err) if err.kind == ErrorKind::Busy => {
                self.shell_pending_queue = true;
            }
            Err(err) => {
                self.shell_pending_queue = false;
                self.record_shell_warning(
                    format!("Could not save UI preferences: {}", err.message),
                    window,
                    cx,
                );
            }
        }
    }

    fn ensure_shell_flush_waiter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_flush_in_flight || self.shutting_down {
            return;
        }
        let Some(store) = self.shell_store.clone() else {
            return;
        };
        self.shell_flush_in_flight = true;
        cx.spawn_in(window, async move |weak, cx| {
            let flush_res = cx
                .background_executor()
                .spawn(async move { flush_and_shutdown_shell(&store, None, false) })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.shell_flush_in_flight = false;
                if let Err(err) = flush_res {
                    this.record_shell_warning(
                        format!("UI preferences write warning: {}", err.message),
                        window,
                        cx,
                    );
                }
                if this.shell_pending_queue && !this.shutting_down {
                    this.queue_shell_persistence(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_prefs_to_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = self.current_view_prefs();
        if let Some(TabSlot::Editable(ed)) = self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        {
            ed.apply_view_prefs(prefs, window, cx);
        }
    }

    pub fn record_recent_file(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.shell_state.recent_files.clone();
        self.shell_state.record_recent(path.clone());
        if !self.shell_loaded {
            if let Some(existing) = self.shell_early_recent.iter().position(|p| p == &path) {
                self.shell_early_recent.remove(existing);
            }
            self.shell_early_recent.insert(0, path);
            if self.shell_early_recent.len() > editor_services::MAX_RECENT_FILES {
                self.shell_early_recent
                    .truncate(editor_services::MAX_RECENT_FILES);
            }
        }
        if self.shell_state.recent_files != before {
            self.queue_shell_persistence(window, cx);
            cx.notify();
        }
    }

    pub fn clear_recent_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shell_loaded {
            self.shell_early_overrides.recent_cleared = true;
            self.shell_early_recent.clear();
        }
        if !self.shell_state.recent_files.is_empty() {
            self.shell_state.clear_recent();
            self.queue_shell_persistence(window, cx);
            self.status_flash = Some(("Cleared recent files".to_string(), Instant::now()));
            cx.notify();
        }
    }

    pub fn set_sidebar_visible(
        &mut self,
        visible: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell_state.sidebar_visible == visible {
            return;
        }
        self.shell_state.sidebar_visible = visible;
        if !self.shell_loaded {
            self.shell_early_overrides.sidebar_visible = true;
        }
        self.queue_shell_persistence(window, cx);
        cx.notify();
    }

    pub fn set_soft_wrap(&mut self, soft_wrap: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell_state.soft_wrap == soft_wrap {
            return;
        }
        self.shell_state.soft_wrap = soft_wrap;
        if !self.shell_loaded {
            self.shell_early_overrides.soft_wrap = true;
        }
        self.apply_prefs_to_active_tab(window, cx);
        self.queue_shell_persistence(window, cx);
        cx.notify();
    }

    pub fn set_show_whitespaces(
        &mut self,
        show_whitespaces: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shell_state.show_whitespaces == show_whitespaces {
            return;
        }
        self.shell_state.show_whitespaces = show_whitespaces;
        if !self.shell_loaded {
            self.shell_early_overrides.show_whitespaces = true;
        }
        self.apply_prefs_to_active_tab(window, cx);
        self.queue_shell_persistence(window, cx);
        cx.notify();
    }

    pub fn set_indent_prefs(
        &mut self,
        tab_size: u8,
        hard_tabs: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let normalized = match tab_size {
            2 | 4 | 8 => tab_size,
            _ => 2,
        };
        if self.shell_state.tab_size == normalized && self.shell_state.hard_tabs == hard_tabs {
            return;
        }
        self.shell_state.tab_size = normalized;
        self.shell_state.hard_tabs = hard_tabs;
        if !self.shell_loaded {
            self.shell_early_overrides.indent = true;
        }
        self.apply_prefs_to_active_tab(window, cx);
        self.queue_shell_persistence(window, cx);
        cx.notify();
    }

    pub fn set_active_line_ending(
        &mut self,
        line_ending: LineEnding,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() {
            return;
        }
        let Some(doc_id) = self.active_document_id() else {
            return;
        };
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) else {
            return;
        };
        match ed.set_line_ending(line_ending) {
            Ok(outcome) => self.finish_format_change(doc_id, outcome, cx),
            Err(msg) => {
                window.push_notification(Notification::warning(msg), cx);
            }
        }
    }

    pub fn set_active_encoding(
        &mut self,
        encoding: EncodingSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() {
            return;
        }
        let Some(doc_id) = self.active_document_id() else {
            return;
        };
        let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) else {
            return;
        };
        match ed.set_encoding(encoding) {
            Ok(outcome) => self.finish_format_change(doc_id, outcome, cx),
            Err(msg) => {
                window.push_notification(Notification::warning(msg), cx);
            }
        }
    }

    fn finish_format_change(
        &mut self,
        doc_id: DocumentId,
        outcome: FormatChangeOutcome,
        cx: &mut Context<Self>,
    ) {
        if outcome != FormatChangeOutcome::NoOp {
            self.mark_metadata(doc_id, PendingDocChangeKind::Update);
        }
        match outcome {
            FormatChangeOutcome::NoOp => {}
            FormatChangeOutcome::MetadataOnlyUntitled => {
                self.persist_session_layout(cx);
                cx.notify();
            }
            FormatChangeOutcome::RecoveryCommitRequired { new_rev } => {
                let commit_admissible =
                    self.document_commit_admissible(doc_id) && !self.is_lifecycle_busy();
                let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) else {
                    return;
                };
                let doc_id = ed.doc_id;
                let immediate_commit = if commit_admissible && ed.durable_revision.0 == 0 {
                    ed.build_recovery_commit()
                } else {
                    None
                };
                self.last_edit_instant = Some(Instant::now());
                if self.search.invalidate_for_doc_edit(doc_id, new_rev) {
                    self.rebuild_search_item_sizes();
                    self.sync_active_search_decorations(cx);
                }
                if let Some(commit) = immediate_commit
                    && let Some(recovery) = &self.recovery
                {
                    let rev = commit.new_rev;
                    if let Some(TabSlot::Editable(ed)) = self.documents.get_mut(&doc_id) {
                        match recovery.try_commit(commit) {
                            Ok(()) => ed.record_commit_queued(rev),
                            Err(err) if err.kind == ErrorKind::Busy => ed.record_commit_busy(),
                            Err(err) => {
                                self.recovery_degraded_error = Some(err.message);
                            }
                        }
                    }
                }
                self.persist_session_layout(cx);
                cx.notify();
            }
        }
    }

    pub fn set_active_language_override(
        &mut self,
        language_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() {
            return;
        }
        let Some(TabSlot::Editable(ed)) = self
            .active_document_id()
            .and_then(|id| self.documents.get_mut(&id))
        else {
            return;
        };
        if ed.set_language_override(language_id, window, cx) {
            let doc_id = ed.doc_id;
            self.mark_metadata(doc_id, PendingDocChangeKind::Update);
            self.persist_session_layout(cx);
            cx.notify();
        }
    }

    // --- Command Palette & Quick Switcher ---

    pub fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette_with_mode(PaletteMode::Commands, window, cx);
    }

    pub fn open_quick_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette_with_mode(PaletteMode::Files, window, cx);
    }

    fn open_palette_with_mode(
        &mut self,
        mode: PaletteMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_or_dismiss_group_ui(window, cx);
        if !self.palette_visible {
            self.palette_prior_origin = Some(self.capture_overlay_origin(window, cx));
        }
        self.goto_visible = false;
        self.palette_visible = true;
        self.palette_base_mode = mode;
        self.palette_state.update(cx, |state, cx| {
            state.set_query("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub fn set_palette_query(
        &mut self,
        query: impl Into<gpui_kit::SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let q = query.into();
        self.palette_state.update(cx, |state, cx| {
            state.set_query(q, window, cx);
        });
        cx.notify();
    }

    pub fn dismiss_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.palette_visible {
            return;
        }
        self.palette_visible = false;
        let prior = self.palette_prior_origin.take();
        if let Some(origin) = prior {
            self.restore_overlay_origin(origin, window, cx);
        }
        cx.notify();
    }

    pub fn confirm_selected_palette_item(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(index_path) = self.palette_state.read(cx).selected_index() else {
            return false;
        };
        self.confirm_palette_index_path(index_path, window, cx)
    }

    pub(crate) fn confirm_palette_index_path(
        &mut self,
        index_path: IndexPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((_, target)) = self
            .palette_targets
            .iter()
            .find(|(path, _)| *path == index_path)
            .cloned()
        else {
            return false;
        };

        if target == PaletteCommandTarget::SwitchToFileMode {
            self.palette_base_mode = PaletteMode::Files;
            self.palette_state.update(cx, |state, cx| {
                state.set_query("", window, cx);
                state.focus(window, cx);
            });
            cx.notify();
            return true;
        }

        self.palette_visible = false;
        let origin = self
            .palette_prior_origin
            .take()
            .unwrap_or_else(|| self.capture_overlay_origin(window, cx));
        let document_command = matches!(
            target,
            PaletteCommandTarget::SaveFile
                | PaletteCommandTarget::SaveFileAs
                | PaletteCommandTarget::ReloadFromDisk
                | PaletteCommandTarget::CloseActiveTab
                | PaletteCommandTarget::GoToLineOrByte
                | PaletteCommandTarget::FindInDocument
                | PaletteCommandTarget::FindAndReplace
                | PaletteCommandTarget::EditUndo
                | PaletteCommandTarget::EditRedo
                | PaletteCommandTarget::EditSelectAll
                | PaletteCommandTarget::IndentSelection
                | PaletteCommandTarget::OutdentSelection
                | PaletteCommandTarget::DuplicateLine
                | PaletteCommandTarget::DeleteLine
                | PaletteCommandTarget::MoveLineUp
                | PaletteCommandTarget::MoveLineDown
                | PaletteCommandTarget::ToggleLineComment
                | PaletteCommandTarget::SetLineEnding(_)
                | PaletteCommandTarget::SetEncoding(_)
                | PaletteCommandTarget::SetLanguageAuto
                | PaletteCommandTarget::SetLanguage(_)
        );
        if document_command
            && origin
                .view
                .is_none_or(|view| self.session.resolve(view).is_err())
        {
            self.notify_error(
                ServiceError::new(
                    ErrorKind::Stale,
                    "The palette's document is no longer open; command cancelled",
                ),
                window,
                cx,
            );
            self.restore_overlay_origin(origin, window, cx);
            cx.notify();
            return false;
        }
        if let Some(view) = origin.view {
            if self.session.resolve(view).is_ok()
                && self.active_view_id() != Some(view.view_id)
                && !self.is_lifecycle_busy()
            {
                self.activate_view(view, window, cx);
            }
        }
        self.restore_overlay_origin(
            OverlayOrigin {
                focus: OverlayFocus::Editor,
                ..origin
            },
            window,
            cx,
        );
        if self.is_lifecycle_busy() && !matches!(target, PaletteCommandTarget::GroupUi(_)) {
            return false;
        }

        match target {
            PaletteCommandTarget::ActivateOpenTab(target) => {
                self.activate_view(target, window, cx);
            }
            PaletteCommandTarget::OpenRecentPath(path) => {
                self.open_path(path, window, cx);
            }
            PaletteCommandTarget::SwitchToFileMode => {}
            PaletteCommandTarget::GroupUi(action) => {
                self.execute_group_palette_action(action, origin, window, cx)
            }
            PaletteCommandTarget::NewTab => {
                self.create_untitled_tab(true, window, cx);
            }
            PaletteCommandTarget::OpenFileDialog => {
                self.on_open_file(&OpenFile, window, cx);
            }
            PaletteCommandTarget::SaveFile => {
                if let Some(target) = origin.view {
                    self.save_view(target, None, false, false, window, cx);
                }
            }
            PaletteCommandTarget::SaveFileAs => {
                if let Some(target) = origin.view {
                    self.prompt_save_view_as(target, false, window, cx);
                }
            }
            PaletteCommandTarget::ReloadFromDisk => {
                if let Some(target) = origin.view {
                    self.request_reload_view(target, window, cx);
                }
            }
            PaletteCommandTarget::ReopenClosedTab => {
                self.reopen_recently_closed(window, cx);
            }
            PaletteCommandTarget::CloseActiveTab => {
                if let Some(target) = origin.view {
                    self.request_close_view(target, window, cx);
                }
            }
            PaletteCommandTarget::ClearRecentFiles => {
                self.clear_recent_files(window, cx);
            }
            PaletteCommandTarget::GoToLineOrByte => {
                self.on_open_goto(&OpenGoToOffset, window, cx);
            }
            PaletteCommandTarget::FindInDocument => {
                self.open_search_bar(false, window, cx);
            }
            PaletteCommandTarget::FindAndReplace => {
                self.open_search_bar(true, window, cx);
            }
            PaletteCommandTarget::EditUndo => {
                if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                    ed.undo(window, cx);
                }
            }
            PaletteCommandTarget::EditRedo => {
                if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                    ed.redo(window, cx);
                }
            }
            PaletteCommandTarget::EditSelectAll => {
                if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                    ed.focus(window, cx);
                    window.dispatch_action(Box::new(SelectAll), cx);
                }
            }
            PaletteCommandTarget::IndentSelection => {
                if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                    ed.focus(window, cx);
                    window.dispatch_action(Box::new(Indent), cx);
                }
            }
            PaletteCommandTarget::OutdentSelection => {
                if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                    ed.focus(window, cx);
                    window.dispatch_action(Box::new(Outdent), cx);
                }
            }
            PaletteCommandTarget::DuplicateLine => {
                self.execute_line_command(Some(LineOpKind::Duplicate), false, window, cx);
            }
            PaletteCommandTarget::DeleteLine => {
                self.execute_line_command(Some(LineOpKind::Delete), false, window, cx);
            }
            PaletteCommandTarget::MoveLineUp => {
                self.execute_line_command(Some(LineOpKind::MoveUp), false, window, cx);
            }
            PaletteCommandTarget::MoveLineDown => {
                self.execute_line_command(Some(LineOpKind::MoveDown), false, window, cx);
            }
            PaletteCommandTarget::ToggleLineComment => {
                self.execute_line_command(None, true, window, cx);
            }
            PaletteCommandTarget::ToggleSidebar => {
                self.set_sidebar_visible(!self.shell_state.sidebar_visible, window, cx);
            }
            PaletteCommandTarget::ToggleSoftWrap => {
                self.set_soft_wrap(!self.shell_state.soft_wrap, window, cx);
            }
            PaletteCommandTarget::ToggleShowWhitespace => {
                self.set_show_whitespaces(!self.shell_state.show_whitespaces, window, cx);
            }
            PaletteCommandTarget::ToggleTheme => {
                self.on_toggle_theme(&ToggleThemeMode, window, cx);
            }
            PaletteCommandTarget::SetIndent {
                tab_size,
                hard_tabs,
            } => {
                self.set_indent_prefs(tab_size, hard_tabs, window, cx);
            }
            PaletteCommandTarget::SetLineEnding(eol) => {
                self.set_active_line_ending(eol, window, cx);
            }
            PaletteCommandTarget::SetEncoding(enc) => {
                self.set_active_encoding(enc, window, cx);
            }
            PaletteCommandTarget::SetLanguageAuto => {
                self.set_active_language_override(None, window, cx);
            }
            PaletteCommandTarget::SetLanguage(id) => {
                self.set_active_language_override(Some(id), window, cx);
            }
        }
        cx.notify();
        true
    }

    // --- Active-Selection Line Commands ---

    pub fn execute_line_command(
        &mut self,
        kind: Option<LineOpKind>,
        is_toggle_comment: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mutation_allowed() {
            return;
        }
        let Some(TabSlot::Editable(ed)) = self.active_slot() else {
            window.push_notification(
                Notification::info("Line commands require an active Editable document."),
                cx,
            );
            return;
        };
        if ed.is_readonly() {
            window.push_notification(
                Notification::warning("Document is read-only; line commands are disabled."),
                cx,
            );
            return;
        }

        let (selection, has_secondary_nonempty) = {
            let state = ed.editor().read(cx);
            let sel = state.selected_range();
            let secondary = sel.is_empty() && state.is_copyable();
            (sel, secondary)
        };

        let base_rev = ed.visible_revision;
        let plan_res = if is_toggle_comment {
            plan_toggle_comment_for_language(
                ed.committed_rope(),
                selection,
                ed.meta.line_ending,
                has_secondary_nonempty,
                ed.language.id,
                ed.language.label,
            )
        } else if let Some(k) = kind {
            plan_line_op(
                ed.committed_rope(),
                selection,
                ed.meta.line_ending,
                has_secondary_nonempty,
                k,
            )
        } else {
            return;
        };

        match plan_res {
            Ok(LineOpPlan::NoOp) => {}
            Ok(LineOpPlan::Apply {
                edits,
                new_selection,
            }) => match self.apply_active_atomic_edits(base_rev, &edits, window, cx) {
                Ok(_) => {
                    if let Some(TabSlot::Editable(ed)) = self.active_slot() {
                        let len = ed.committed_rope().len();
                        let s = new_selection.start.min(len);
                        let e = new_selection.end.min(len).max(s);
                        ed.editor().update(cx, |state, cx| {
                            state.set_selected_range(s..e, cx);
                            state.focus(window, cx);
                        });
                    }
                }
                Err(err) => {
                    window.push_notification(
                        Notification::warning(format!("Line command rejected: {err}")),
                        cx,
                    );
                }
            },
            Err(err) => {
                let msg = err.to_string();
                self.status_flash = Some((msg.clone(), Instant::now()));
                window.push_notification(Notification::warning(msg), cx);
                cx.notify();
            }
        }
    }

    // --- New Action Listeners ---

    pub(crate) fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar_visible(!self.shell_state.sidebar_visible, window, cx);
    }

    pub(crate) fn on_open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_visible && self.palette_base_mode == PaletteMode::Commands {
            self.dismiss_palette(window, cx);
        } else {
            self.open_command_palette(window, cx);
        }
    }

    pub(crate) fn on_open_quick_switcher(
        &mut self,
        _: &OpenQuickSwitcher,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_visible && self.palette_base_mode == PaletteMode::Files {
            self.dismiss_palette(window, cx);
        } else {
            self.open_quick_switcher(window, cx);
        }
    }

    pub(crate) fn on_clear_recent_files(
        &mut self,
        _: &ClearRecentFiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_recent_files(window, cx);
    }

    pub(crate) fn on_duplicate_line(
        &mut self,
        _: &DuplicateLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_line_command(Some(LineOpKind::Duplicate), false, window, cx);
    }

    pub(crate) fn on_delete_line(
        &mut self,
        _: &DeleteLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_line_command(Some(LineOpKind::Delete), false, window, cx);
    }

    pub(crate) fn on_move_line_up(
        &mut self,
        _: &MoveLineUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_line_command(Some(LineOpKind::MoveUp), false, window, cx);
    }

    pub(crate) fn on_move_line_down(
        &mut self,
        _: &MoveLineDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_line_command(Some(LineOpKind::MoveDown), false, window, cx);
    }

    pub(crate) fn on_toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_line_command(None, true, window, cx);
    }

    pub(crate) fn on_toggle_soft_wrap(
        &mut self,
        _: &ToggleSoftWrap,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_soft_wrap(!self.shell_state.soft_wrap, window, cx);
    }

    pub(crate) fn on_toggle_show_whitespace(
        &mut self,
        _: &ToggleShowWhitespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_show_whitespaces(!self.shell_state.show_whitespaces, window, cx);
    }

    pub(crate) fn on_set_indent_spaces_2(
        &mut self,
        _: &SetIndentSpaces2,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(2, false, window, cx);
    }

    pub(crate) fn on_set_indent_spaces_4(
        &mut self,
        _: &SetIndentSpaces4,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(4, false, window, cx);
    }

    pub(crate) fn on_set_indent_spaces_8(
        &mut self,
        _: &SetIndentSpaces8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(8, false, window, cx);
    }

    pub(crate) fn on_set_indent_tabs_2(
        &mut self,
        _: &SetIndentTabs2,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(2, true, window, cx);
    }

    pub(crate) fn on_set_indent_tabs_4(
        &mut self,
        _: &SetIndentTabs4,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(4, true, window, cx);
    }

    pub(crate) fn on_set_indent_tabs_8(
        &mut self,
        _: &SetIndentTabs8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_indent_prefs(8, true, window, cx);
    }

    pub(crate) fn on_set_eol_lf(
        &mut self,
        _: &SetLineEndingLf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_line_ending(LineEnding::Lf, window, cx);
    }

    pub(crate) fn on_set_eol_crlf(
        &mut self,
        _: &SetLineEndingCrlf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_line_ending(LineEnding::Crlf, window, cx);
    }

    pub(crate) fn on_set_encoding_utf8(
        &mut self,
        _: &SetEncodingUtf8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_encoding(EncodingSpec::Utf8, window, cx);
    }

    pub(crate) fn on_set_encoding_utf8_bom(
        &mut self,
        _: &SetEncodingUtf8Bom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_encoding(EncodingSpec::Utf8Bom, window, cx);
    }

    pub(crate) fn on_select_lang_auto(
        &mut self,
        _: &SelectLanguageAuto,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(None, window, cx);
    }

    pub(crate) fn on_select_lang_plaintext(
        &mut self,
        _: &SelectLangPlaintext,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("plaintext"), w, cx);
    }
    pub(crate) fn on_select_lang_rust(
        &mut self,
        _: &SelectLangRust,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("rust"), w, cx);
    }
    pub(crate) fn on_select_lang_toml(
        &mut self,
        _: &SelectLangToml,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("toml"), w, cx);
    }
    pub(crate) fn on_select_lang_json(
        &mut self,
        _: &SelectLangJson,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("json"), w, cx);
    }
    pub(crate) fn on_select_lang_typescript(
        &mut self,
        _: &SelectLangTypescript,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("typescript"), w, cx);
    }
    pub(crate) fn on_select_lang_tsx(
        &mut self,
        _: &SelectLangTsx,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("tsx"), w, cx);
    }
    pub(crate) fn on_select_lang_javascript(
        &mut self,
        _: &SelectLangJavascript,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("javascript"), w, cx);
    }
    pub(crate) fn on_select_lang_python(
        &mut self,
        _: &SelectLangPython,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("python"), w, cx);
    }
    pub(crate) fn on_select_lang_markdown(
        &mut self,
        _: &SelectLangMarkdown,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("markdown"), w, cx);
    }
    pub(crate) fn on_select_lang_yaml(
        &mut self,
        _: &SelectLangYaml,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("yaml"), w, cx);
    }
    pub(crate) fn on_select_lang_html(
        &mut self,
        _: &SelectLangHtml,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("html"), w, cx);
    }
    pub(crate) fn on_select_lang_css(
        &mut self,
        _: &SelectLangCss,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("css"), w, cx);
    }
    pub(crate) fn on_select_lang_sql(
        &mut self,
        _: &SelectLangSql,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("sql"), w, cx);
    }
    pub(crate) fn on_select_lang_bash(
        &mut self,
        _: &SelectLangBash,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("bash"), w, cx);
    }
    pub(crate) fn on_select_lang_c(
        &mut self,
        _: &SelectLangC,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("c"), w, cx);
    }
    pub(crate) fn on_select_lang_cpp(
        &mut self,
        _: &SelectLangCpp,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("cpp"), w, cx);
    }
    pub(crate) fn on_select_lang_go(
        &mut self,
        _: &SelectLangGo,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("go"), w, cx);
    }
    pub(crate) fn on_select_lang_swift(
        &mut self,
        _: &SelectLangSwift,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("swift"), w, cx);
    }
    pub(crate) fn on_select_lang_xml(
        &mut self,
        _: &SelectLangXml,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("xml"), w, cx);
    }
    pub(crate) fn on_select_lang_make(
        &mut self,
        _: &SelectLangMake,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_active_language_override(Some("make"), w, cx);
    }
}

// Keep unused `Enter` import check clean if referenced in render layer.
const _: Option<Enter> = None;

/// Off-UI-thread helper for flushing or shutting down `UiShellStore` with bounded retries.
fn flush_and_shutdown_shell(
    store: &UiShellStore,
    pending_snapshot: Option<UiShellState>,
    shutdown: bool,
) -> Result<(), ServiceError> {
    if let Some(state) = pending_snapshot {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match store.queue(state.clone()) {
                Ok(()) => break,
                Err(e) if e.kind == ErrorKind::Busy && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(e),
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    let rx = loop {
        let op = if shutdown {
            store.shutdown()
        } else {
            store.flush()
        };
        match op {
            Ok(rx) => break rx,
            Err(e) if e.kind == ErrorKind::Busy && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    };
    rx.recv_timeout(Duration::from_secs(4)).map_err(|e| {
        ServiceError::new(
            ErrorKind::Storage,
            format!("ui_shell flush/shutdown timeout: {e}"),
        )
    })?
}
