use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use editor_services::{
    CancelToken, ContentIdentity, DocMeta, DocumentId, EncodingSpec, ErrorKind, FaultInjector,
    FileFingerprint, HydratedDraft, IndexStatus, LifecycleGen, LineEnding, OpenOptions,
    OpenOutcome, PagingService, RecoveryEvent, RecoveryHandle, Revision, SaveOutcome, SaveRequest,
    SearchService, SearchTarget, ServiceError, SessionManifest, SessionTabStub, ShellTheme,
    TabLayoutRecord, TryRecvError, UiShellLoad, UiShellState, UiShellStore, ViewState,
    ViewportAnchor, open_document, save_document,
};
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
pub enum TabSlot {
    Dehydrated(SessionTabStub),
    Hydrating {
        stub: SessionTabStub,
        req_seq: u64,
    },
    OverBudgetDraft {
        stub: SessionTabStub,
        reason: String,
    },
    Editable(EditorAdapter),
    Paged(PagedTabState),
    Failed {
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        meta: DocMeta,
        error: String,
    },
}

impl TabSlot {
    pub fn doc_id(&self) -> DocumentId {
        match self {
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. } => s.doc_id,
            Self::Editable(ed) => ed.doc_id,
            Self::Paged(pv) => pv.doc_id,
            Self::Failed { doc_id, .. } => *doc_id,
        }
    }

    pub fn lifecycle_gen(&self) -> LifecycleGen {
        match self {
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. } => s.lifecycle_gen,
            Self::Editable(ed) => ed.lifecycle_gen,
            Self::Paged(pv) => pv.lifecycle_gen,
            Self::Failed { lifecycle_gen, .. } => *lifecycle_gen,
        }
    }

    pub fn meta(&self) -> &DocMeta {
        match self {
            Self::Dehydrated(s)
            | Self::Hydrating { stub: s, .. }
            | Self::OverBudgetDraft { stub: s, .. } => &s.meta,
            Self::Editable(ed) => &ed.meta,
            Self::Paged(pv) => &pv.meta,
            Self::Failed { meta, .. } => meta,
        }
    }

    pub fn title(&self) -> &str {
        let t = self.meta().title.as_str();
        if t.is_empty() { "untitled" } else { t }
    }

    pub fn is_dirty(&self) -> bool {
        match self {
            Self::Dehydrated(stub)
            | Self::Hydrating { stub, .. }
            | Self::OverBudgetDraft { stub, .. } => stub.has_checkpoint,
            Self::Editable(ed) => ed.is_dirty(),
            Self::Paged(_) | Self::Failed { .. } => false,
        }
    }

    /// Honest metadata-only badge descriptor for sidebar, tab strip, and quick switcher:
    /// never claims `MOD` for a file-backed unhydrated `SessionTabStub` unless metadata itself
    /// proves format-dirty (`Main` contract note: `has_checkpoint` may match saved disk content).
    pub fn honest_badge(&self) -> HonestSlotBadge {
        match self {
            Self::Editable(ed) => {
                if ed.is_readonly() {
                    HonestSlotBadge {
                        short_badge: Some("RO"),
                        detail_hint: Some("Read-only"),
                        proven_modified: false,
                    }
                } else if ed.is_dirty() {
                    HonestSlotBadge {
                        short_badge: Some("MOD"),
                        detail_hint: Some("Modified"),
                        proven_modified: true,
                    }
                } else {
                    HonestSlotBadge {
                        short_badge: None,
                        detail_hint: None,
                        proven_modified: false,
                    }
                }
            }
            Self::Paged(_) => HonestSlotBadge {
                short_badge: Some("PG"),
                detail_hint: Some("Paged read-only"),
                proven_modified: false,
            },
            Self::Dehydrated(stub) => {
                describe_unhydrated_stub_badge(&stub.meta, stub.has_checkpoint)
            }
            Self::Hydrating { stub, .. } => {
                let base = describe_unhydrated_stub_badge(&stub.meta, stub.has_checkpoint);
                HonestSlotBadge {
                    short_badge: Some("…"),
                    detail_hint: Some("Loading…"),
                    proven_modified: base.proven_modified,
                }
            }
            Self::OverBudgetDraft { .. } => HonestSlotBadge {
                short_badge: Some("CAP"),
                detail_hint: Some("Protected over-budget draft"),
                proven_modified: true,
            },
            Self::Failed { .. } => HonestSlotBadge {
                short_badge: Some("ERR"),
                detail_hint: Some("Load error"),
                proven_modified: false,
            },
        }
    }

    pub fn layout_record(&self, cx: &App) -> TabLayoutRecord {
        match self {
            Self::Dehydrated(stub)
            | Self::Hydrating { stub, .. }
            | Self::OverBudgetDraft { stub, .. } => TabLayoutRecord {
                doc_id: stub.doc_id,
                lifecycle_gen: stub.lifecycle_gen,
                meta: stub.meta.clone(),
                view: stub.view.clone(),
            },
            Self::Editable(ed) => TabLayoutRecord {
                doc_id: ed.doc_id,
                lifecycle_gen: ed.lifecycle_gen,
                meta: ed.meta.clone(),
                view: ed.capture_view_state(cx),
            },
            Self::Paged(pv) => TabLayoutRecord {
                doc_id: pv.doc_id,
                lifecycle_gen: pv.lifecycle_gen,
                meta: pv.meta.clone(),
                view: pv.capture_view_state(),
            },
            Self::Failed {
                doc_id,
                lifecycle_gen,
                meta,
                ..
            } => TabLayoutRecord {
                doc_id: *doc_id,
                lifecycle_gen: *lifecycle_gen,
                meta: meta.clone(),
                view: ViewState::default(),
            },
        }
    }
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
    pub(crate) faults: FaultInjector,

    pub(crate) budget_config: HydrationBudgetConfig,
    pub(crate) activation_clock: u64,
    pub(crate) untitled_counter: usize,
    pub(crate) hydrate_req_counter: u64,

    pub(crate) tabs: Vec<TabSlot>,
    pub(crate) active_tab: usize,
    pub(crate) recently_closed: Vec<SessionTabStub>,

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
    pub(crate) palette_prior_focus: Option<FocusHandle>,
    pub(crate) palette_targets: Vec<(IndexPath, PaletteCommandTarget)>,

    pub(crate) status_flash: Option<(String, Instant)>,
    pub(crate) last_edit_instant: Option<Instant>,
    pub(crate) shutting_down: bool,
    pub(crate) quit_confirmed: bool,
    pub shutdown_durable_ack: Arc<AtomicBool>,
    pub(crate) closing_docs: HashMap<DocumentId, bool>,
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
            faults: FaultInjector::default(),
            budget_config,
            activation_clock: 1,
            untitled_counter: 0,
            hydrate_req_counter: 0,
            tabs: Vec::new(),
            active_tab: 0,
            recently_closed: Vec::new(),
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
            palette_prior_focus: None,
            palette_targets: Vec::new(),
            status_flash: None,
            last_edit_instant: None,
            shutting_down: false,
            quit_confirmed: false,
            shutdown_durable_ack: Arc::new(AtomicBool::new(false)),
            closing_docs: HashMap::new(),
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

    fn start_background_bootstrap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let db_path = self.cli.data_dir.join("recovery.sqlite3");
        let shell_path = self.cli.data_dir.join("ui_shell.json");
        let cli_files = self.cli.files.clone();
        let faults = self.faults.clone();

        // Background UI shell preferences & MRU bootstrap (never blocks first frame or recovery).
        cx.spawn_in(window, async move |weak, cx| {
            let shell_boot = cx
                .background_executor()
                .spawn(async move { UiShellStore::open(&shell_path) })
                .await;
            let _ = weak.update_in(cx, |this, window, cx| {
                this.apply_loaded_shell_bootstrap(shell_boot, window, cx);
            });
        })
        .detach();

        cx.spawn_in(window, async move |weak, cx| {
            let boot = cx
                .background_executor()
                .spawn(async move {
                    let handle = RecoveryHandle::start_with_faults(&db_path, faults)?;
                    let open_manifest = handle
                        .request_manifest()?
                        .recv_timeout(Duration::from_secs(10))
                        .map_err(|e| {
                            ServiceError::new(ErrorKind::Storage, format!("manifest timeout: {e}"))
                        })??;
                    let closed_manifest = handle
                        .request_recently_closed()
                        .ok()
                        .and_then(|rx| rx.recv_timeout(Duration::from_secs(5)).ok())
                        .and_then(Result::ok)
                        .unwrap_or_default();
                    Ok::<(RecoveryHandle, SessionManifest, SessionManifest), ServiceError>((
                        handle,
                        open_manifest,
                        closed_manifest,
                    ))
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.startup_metrics.recovery_manifest_ms =
                    Some(this.process_start.elapsed().as_secs_f64() * 1000.0);
                match boot {
                    Ok((handle, manifest, closed)) => {
                        this.recovery = Some(handle);
                        this.recovery_ready = true;
                        for warn in manifest.warnings.iter().chain(closed.warnings.iter()) {
                            let label = match warn.doc_id {
                                Some(id) => format!("Recovery warning ({:08x}): {}", id.0 as u32, warn.message),
                                None => format!("Recovery warning: {}", warn.message),
                            };
                            if this.recovery_degraded_error.is_none() {
                                this.recovery_degraded_error = Some(label.clone());
                            }
                            window.push_notification(Notification::warning(label), cx);
                        }
                        this.recently_closed = closed.tabs;
                        this.apply_recovered_manifest(manifest, &cli_files, window, cx);
                    }
                    Err(err) => {
                        this.recovery_ready = true;
                        this.recovery_degraded_error = Some(err.message.clone());
                        window.push_notification(
                            Notification::error(format!(
                                "Recovery storage unavailable ({}): editing works in memory, save files explicitly before quitting.",
                                err.message
                            )),
                            cx,
                        );
                    }
                }

                for path in cli_files {
                    this.open_path(path, window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_recovered_manifest(
        &mut self,
        manifest: SessionManifest,
        cli_files: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if manifest.tabs.is_empty() {
            this_persist_layout_if_clean(self, cx);
            return;
        }

        // Replace untouched initial placeholder tab if the user has not typed into it yet.
        let placeholder_untouched = self.tabs.len() == 1
            && matches!(
                &self.tabs[0],
                TabSlot::Editable(ed)
                    if ed.meta.path.is_none()
                        && ed.visible_revision == Revision(0)
                        && ed.committed_byte_len() == 0
                        && ed.meta.encoding == EncodingSpec::Utf8
                        && ed.meta.line_ending == LineEnding::Lf
                        && ed.meta.language_override.is_none()
            );
        let base_index = if placeholder_untouched {
            self.tabs.clear();
            0
        } else {
            self.tabs.len()
        };

        let mut target_active = base_index;
        for (offset, stub) in manifest.tabs.into_iter().enumerate() {
            if Some(stub.doc_id) == manifest.active_doc {
                target_active = base_index + offset;
            }
            self.tabs.push(TabSlot::Dehydrated(stub));
        }

        // CRITICAL (`B1`, `R07`): Only hydrate the single active tab when no CLI files override focus!
        if cli_files.is_empty() && !self.tabs.is_empty() {
            let idx = target_active.min(self.tabs.len() - 1);
            self.activate_tab(idx, window, cx);
        }
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
                        if let Some(TabSlot::Editable(ed)) =
                            self.tabs.iter_mut().find(|t| t.doc_id() == doc_id)
                        {
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
            if !self.shutting_down {
                for slot in &mut self.tabs {
                    let TabSlot::Editable(ed) = slot else {
                        continue;
                    };
                    if self.closing_docs.contains_key(&ed.doc_id) {
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

        if self.layout_dirty {
            self.persist_session_layout(cx);
        }
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

    pub fn hydrated_editor_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|slot| matches!(slot, TabSlot::Editable(_)))
            .count()
    }

    pub fn hydrated_content_bytes(&self) -> u64 {
        self.tabs
            .iter()
            .filter_map(|slot| match slot {
                TabSlot::Editable(ed) => Some(ed.committed_byte_len()),
                _ => None,
            })
            .sum()
    }

    fn collect_hydrated_slots(&self) -> Vec<HydratedSlotInfo> {
        self.tabs
            .iter()
            .enumerate()
            .filter_map(|(idx, slot)| match slot {
                TabSlot::Editable(ed) => Some(HydratedSlotInfo {
                    tab_index: idx,
                    byte_len: ed.committed_byte_len(),
                    is_dirty: ed.is_dirty(),
                    has_undo_history: ed.has_undo_history(),
                    is_file_backed: ed.meta.path.is_some(),
                    is_active: idx == self.active_tab,
                    last_activated_tick: ed.last_activated_tick,
                }),
                _ => None,
            })
            .collect()
    }

    /// Reclaims clean, unedited (`!is_dirty && !has_undo_history`) inactive tabs selected by
    /// `plan_hydration_admission`, preserving cursor/view metadata in `TabSlot::Dehydrated`.
    fn dehydrate_clean_tabs(&mut self, indices: &[usize], cx: &mut Context<Self>) {
        for &idx in indices {
            if idx >= self.tabs.len() || idx == self.active_tab {
                continue;
            }
            let can_dehydrate = matches!(
                &self.tabs[idx],
                TabSlot::Editable(ed) if !ed.is_dirty() && !ed.has_undo_history() && ed.meta.path.is_some()
            );
            if !can_dehydrate {
                continue;
            }
            if let TabSlot::Editable(ed) = &self.tabs[idx] {
                let stub = SessionTabStub {
                    doc_id: ed.doc_id,
                    lifecycle_gen: ed.lifecycle_gen,
                    durable_rev: ed.durable_revision,
                    meta: ed.meta.clone(),
                    view: ed.capture_view_state(cx),
                    has_checkpoint: false,
                };
                self.tabs[idx] = TabSlot::Dehydrated(stub);
            }
        }
    }

    fn next_activation_tick(&mut self) -> u64 {
        self.activation_clock = self.activation_clock.wrapping_add(1);
        self.activation_clock
    }

    pub fn create_untitled_tab(
        &mut self,
        persist_layout: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DocumentId {
        let slots = self.collect_hydrated_slots();
        if let AdmissionDecision::Admit {
            dehydrate_tab_indices,
        } = plan_hydration_admission(self.budget_config, &slots, 0)
        {
            self.dehydrate_clean_tabs(&dehydrate_tab_indices, cx);
        } else if self.hydrated_editor_count() >= self.budget_config.max_hydrated_editors {
            window.push_notification(
                Notification::warning(format!(
                    "Cannot create another live editor: {} dirty/edited tabs already occupy the {}-editor cap.",
                    self.hydrated_editor_count(),
                    self.budget_config.max_hydrated_editors
                )),
                cx,
            );
            return self
                .tabs
                .get(self.active_tab)
                .map(TabSlot::doc_id)
                .unwrap_or_else(DocumentId::new);
        }

        self.untitled_counter += 1;
        let doc_id = DocumentId::new();
        let meta = DocMeta {
            title: format!("untitled-{}", self.untitled_counter),
            path: None,
            encoding: EncodingSpec::Utf8,
            line_ending: LineEnding::Lf,
            origin_fingerprint: None,
            saved_identity: None,
            readonly_reason: None,
            paged: false,
            saved_encoding: None,
            saved_line_ending: None,
            language_override: None,
        };
        let tick = self.next_activation_tick();
        let initial_prefs = self.current_view_prefs();
        let mut adapter = EditorAdapter::new(
            HydrateEditorParams {
                doc_id,
                lifecycle_gen: LifecycleGen(1),
                meta,
                rope: Rope::new(),
                initial_rev: Revision(0),
                durable_rev: Revision(0),
                is_clean_at_load: true,
                disk_conflict: false,
                initial_view: ViewState::default(),
                initial_prefs,
                activation_tick: tick,
            },
            window,
            cx,
        );
        let sub = self.subscribe_editor(&adapter, cx);
        adapter.attach_subscription(sub);
        adapter.focus(window, cx);

        self.tabs.push(TabSlot::Editable(adapter));
        self.active_tab = self.tabs.len() - 1;
        if persist_layout {
            self.persist_session_layout(cx);
        }
        cx.notify();
        doc_id
    }

    fn subscribe_editor(&self, adapter: &EditorAdapter, cx: &mut Context<Self>) -> Subscription {
        let doc_id = adapter.doc_id;
        cx.subscribe(
            adapter.editor(),
            move |this: &mut Self, _editor, event: &InputEditsEvent, cx| {
                this.on_editor_committed_edits(doc_id, event, cx);
            },
        )
    }

    fn on_editor_committed_edits(
        &mut self,
        doc_id: DocumentId,
        event: &InputEditsEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(slot_ix) = self.tabs.iter().position(|s| s.doc_id() == doc_id) else {
            return;
        };
        let TabSlot::Editable(ed) = &mut self.tabs[slot_ix] else {
            return;
        };

        let Some(outcome) = ed.ingest_edits_event(event) else {
            return;
        };
        self.finish_committed_mutation(slot_ix, outcome, cx);
    }

    fn finish_committed_mutation(
        &mut self,
        slot_ix: usize,
        outcome: CommittedMutationOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) else {
            return;
        };
        let doc_id = ed.doc_id;
        let first_baseline = ed.durable_revision.0 == 0 && ed.is_dirty();
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
            if let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) {
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
                doc_id,
                seq,
                checked_text_seq,
                saved_identity,
                outcome.post_rope,
                cx,
            );
        }

        cx.notify();
    }

    fn spawn_savepoint_verification(
        &self,
        doc_id: DocumentId,
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
                if let Some(TabSlot::Editable(ed)) =
                    this.tabs.iter_mut().find(|t| t.doc_id() == doc_id)
                {
                    ed.apply_savepoint_verification(seq, checked_text_seq, matches);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn activate_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        // Flush previous active tab's pending commit when switching tabs.
        self.flush_tab_commit(self.active_tab);

        self.active_tab = index;
        let tick = self.next_activation_tick();
        let current_prefs = self.current_view_prefs();
        match &mut self.tabs[index] {
            TabSlot::Editable(ed) => {
                ed.last_activated_tick = tick;
                ed.apply_view_prefs(current_prefs, window, cx);
                ed.focus(window, cx);
            }
            TabSlot::Paged(pv) => {
                if pv.current_slice.is_none() && !pv.is_loading {
                    let offset = pv.current_anchor_byte;
                    self.request_paged_viewport(
                        index,
                        ViewportAnchor::ByteOffset(offset),
                        window,
                        cx,
                    );
                }
                self.focus_handle.focus(window, cx);
            }
            TabSlot::Dehydrated(stub) => {
                let stub_clone = stub.clone();
                self.start_hydrating_stub(index, stub_clone, window, cx);
            }
            TabSlot::Hydrating { .. }
            | TabSlot::OverBudgetDraft { .. }
            | TabSlot::Failed { .. } => {
                self.focus_handle.focus(window, cx);
            }
        }

        if self.search.visible {
            self.run_search_from_inputs(window, cx);
        }
        self.persist_session_layout(cx);
        cx.notify();
    }

    pub(crate) fn start_hydrating_stub(
        &mut self,
        index: usize,
        stub: SessionTabStub,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.hydrate_req_counter = self.hydrate_req_counter.wrapping_add(1);
        let req_seq = self.hydrate_req_counter;
        let doc_id = stub.doc_id;
        self.tabs[index] = TabSlot::Hydrating {
            stub: stub.clone(),
            req_seq,
        };

        match classify_stub_hydration_source(stub.has_checkpoint, stub.meta.path.is_some()) {
            // Case A1: Paged or clean disk-backed stub without unsaved recovery checkpoint -> open from disk.
            StubHydrationSource::DiskOpen => {
                if let Some(path) = stub.meta.path.clone() {
                    self.spawn_disk_open_into_slot(
                        doc_id,
                        stub.lifecycle_gen,
                        Some(stub.view),
                        path,
                        false,
                        window,
                        cx,
                    );
                }
            }
            // Case A2: Clean empty untitled stub (`!has_checkpoint && path.is_none()`) -> reconstitute
            // directly from layout metadata without querying `recovery_heads` (`QueryReturnedNoRows`).
            StubHydrationSource::EmptyCleanUntitled => {
                let slots = self.collect_hydrated_slots();
                match plan_hydration_admission(self.budget_config, &slots, 0) {
                    AdmissionDecision::Admit {
                        dehydrate_tab_indices,
                    } => {
                        self.dehydrate_clean_tabs(&dehydrate_tab_indices, cx);
                        let tick = self.next_activation_tick();
                        let initial_prefs = self.current_view_prefs();
                        let mut adapter = EditorAdapter::new(
                            HydrateEditorParams {
                                doc_id,
                                lifecycle_gen: stub.lifecycle_gen,
                                meta: stub.meta,
                                rope: Rope::new(),
                                initial_rev: Revision(0),
                                durable_rev: Revision(0),
                                is_clean_at_load: true,
                                disk_conflict: false,
                                initial_view: stub.view,
                                initial_prefs,
                                activation_tick: tick,
                            },
                            window,
                            cx,
                        );
                        let sub = self.subscribe_editor(&adapter, cx);
                        adapter.attach_subscription(sub);
                        if index == self.active_tab {
                            adapter.focus(window, cx);
                        }
                        self.tabs[index] = TabSlot::Editable(adapter);
                    }
                    AdmissionDecision::OverBudget { reason, .. } => {
                        self.tabs[index] = TabSlot::OverBudgetDraft { stub, reason };
                    }
                }
                cx.notify();
            }
            // Case B: Persisted draft (`has_checkpoint == true`) -> hydrate from SQLite recovery worker.
            StubHydrationSource::RecoveryDatabase => {
                let Some(recovery) = self.recovery.clone() else {
                    self.tabs[index] = TabSlot::Failed {
                        doc_id,
                        lifecycle_gen: stub.lifecycle_gen,
                        meta: stub.meta,
                        error: "Recovery service unavailable; cannot hydrate stored draft."
                            .to_string(),
                    };
                    cx.notify();
                    return;
                };

                cx.spawn_in(window, async move |weak, cx| {
                    let hydrated = cx
                        .background_executor()
                        .spawn(async move {
                            let rx = recovery.request_hydrate(doc_id)?;
                            rx.recv_timeout(Duration::from_secs(15)).map_err(|e| {
                                ServiceError::new(
                                    ErrorKind::Storage,
                                    format!("hydrate timeout: {e}"),
                                )
                            })?
                        })
                        .await;

                    let _ = weak.update_in(cx, |this, window, cx| {
                        this.finish_draft_hydration(doc_id, req_seq, hydrated, window, cx);
                    });
                })
                .detach();
            }
        }
    }

    fn finish_draft_hydration(
        &mut self,
        doc_id: DocumentId,
        req_seq: u64,
        result: Result<HydratedDraft, ServiceError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tabs.iter().position(|s| s.doc_id() == doc_id) else {
            return;
        };
        let initial_view = match &self.tabs[idx] {
            TabSlot::Hydrating { stub, req_seq: s } if *s == req_seq => stub.view.clone(),
            _ => return,
        };

        match result {
            Ok(draft) => {
                let incoming_bytes = draft.rope.len() as u64;
                let slots = self.collect_hydrated_slots();
                match plan_hydration_admission(self.budget_config, &slots, incoming_bytes) {
                    AdmissionDecision::Admit {
                        dehydrate_tab_indices,
                    } => {
                        self.dehydrate_clean_tabs(&dehydrate_tab_indices, cx);
                        let tick = self.next_activation_tick();
                        let initial_prefs = self.current_view_prefs();
                        let is_clean_at_load = match draft.meta.saved_identity {
                            Some(saved_id) => saved_id == draft.identity,
                            None => draft.rope.len() == 0,
                        };
                        let mut adapter = EditorAdapter::new(
                            HydrateEditorParams {
                                doc_id: draft.doc_id,
                                lifecycle_gen: draft.lifecycle_gen,
                                meta: draft.meta,
                                rope: draft.rope,
                                initial_rev: draft.durable_rev,
                                durable_rev: draft.durable_rev,
                                is_clean_at_load,
                                disk_conflict: draft.disk_conflict,
                                initial_view,
                                initial_prefs,
                                activation_tick: tick,
                            },
                            window,
                            cx,
                        );
                        let sub = self.subscribe_editor(&adapter, cx);
                        adapter.attach_subscription(sub);
                        let is_active = idx == self.active_tab;
                        if is_active {
                            adapter.focus(window, cx);
                        }
                        self.tabs[idx] = TabSlot::Editable(adapter);
                    }
                    AdmissionDecision::OverBudget { reason, .. } => {
                        // Never delete or overwrite SQLite recovery state; offer Rescue Export!
                        let stub = SessionTabStub {
                            doc_id: draft.doc_id,
                            lifecycle_gen: draft.lifecycle_gen,
                            durable_rev: draft.durable_rev,
                            meta: draft.meta,
                            view: initial_view,
                            has_checkpoint: true,
                        };
                        self.tabs[idx] = TabSlot::OverBudgetDraft { stub, reason };
                    }
                }
            }
            Err(err) => {
                let meta = self.tabs[idx].meta().clone();
                let lifecycle_gen = self.tabs[idx].lifecycle_gen();
                self.tabs[idx] = TabSlot::Failed {
                    doc_id,
                    lifecycle_gen,
                    meta,
                    error: format!("Draft recovery failed: {}", err.message),
                };
            }
        }
        cx.notify();
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let lexical = std::path::absolute(&path).unwrap_or_else(|_| path.clone());

        // Focus existing tab if lexically open (zero UI-thread filesystem `stat`/`canonicalize`).
        if let Some(existing_ix) = self.tabs.iter().position(|slot| {
            slot.meta().path.as_deref() == Some(lexical.as_path())
                || slot.meta().path.as_deref() == Some(path.as_path())
        }) {
            let existing_path = self.tabs[existing_ix]
                .meta()
                .path
                .clone()
                .unwrap_or(lexical);
            self.activate_tab(existing_ix, window, cx);
            self.record_recent_file(existing_path, window, cx);
            return;
        }

        // Reuse untouched initial scratch tab if empty and uncustomized.
        let reuse_scratch = self.tabs.len() == 1
            && matches!(
                &self.tabs[0],
                TabSlot::Editable(ed)
                    if ed.meta.path.is_none()
                        && ed.visible_revision == Revision(0)
                        && ed.committed_byte_len() == 0
                        && !ed.is_dirty()
                        && ed.meta.encoding == EncodingSpec::Utf8
                        && ed.meta.line_ending == LineEnding::Lf
                        && ed.meta.language_override.is_none()
            );
        let doc_id = DocumentId::new();
        let lifecycle_gen = LifecycleGen(1);
        let title = lexical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document")
            .to_string();
        let stub = SessionTabStub {
            doc_id,
            lifecycle_gen,
            durable_rev: Revision(0),
            meta: DocMeta {
                title,
                path: Some(lexical.clone()),
                ..DocMeta::default()
            },
            view: ViewState::default(),
            has_checkpoint: false,
        };

        self.hydrate_req_counter = self.hydrate_req_counter.wrapping_add(1);
        let req_seq = self.hydrate_req_counter;
        let slot = TabSlot::Hydrating { stub, req_seq };

        if reuse_scratch {
            self.tabs[0] = slot;
            self.active_tab = 0;
        } else {
            self.tabs.push(slot);
            self.active_tab = self.tabs.len() - 1;
        }

        self.spawn_disk_open_into_slot(doc_id, lifecycle_gen, None, lexical, true, window, cx);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_disk_open_into_slot(
        &mut self,
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        initial_view: Option<ViewState>,
        path: PathBuf,
        record_in_mru: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paging = self.paging_service.clone();
        let mut options = OpenOptions::default();
        if let Some(limit) = self.cli.editable_byte_limit {
            options.editable_byte_limit = limit;
        }
        let cancel = CancelToken::new();
        let path_for_worker = path;

        cx.spawn_in(window, async move |weak, cx| {
            let (resolved_path, outcome) = cx
                .background_executor()
                .spawn(async move {
                    let resolved =
                        std::fs::canonicalize(&path_for_worker).unwrap_or_else(|_| path_for_worker);
                    let retry_deadline = Instant::now() + Duration::from_secs(30);
                    let res = loop {
                        match open_document(doc_id, &resolved, &paging, options, &cancel) {
                            Err(err)
                                if err.kind == ErrorKind::Busy
                                    && !cancel.is_cancelled()
                                    && Instant::now() < retry_deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(25));
                            }
                            other => break other,
                        }
                    };
                    (resolved, res)
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.finish_disk_open(
                    doc_id,
                    lifecycle_gen,
                    resolved_path,
                    initial_view.unwrap_or_default(),
                    record_in_mru,
                    outcome,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_disk_open(
        &mut self,
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        path: PathBuf,
        initial_view: ViewState,
        record_in_mru: bool,
        result: Result<OpenOutcome, ServiceError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tabs.iter().position(|s| s.doc_id() == doc_id) else {
            return;
        };
        // Deduplicate if background canonicalization resolved a symlink to an already-open tab!
        if record_in_mru
            && let Some(dup_ix) = self.tabs.iter().position(|s| {
                s.doc_id() != doc_id && s.meta().path.as_deref() == Some(path.as_path())
            })
        {
            let was_active = idx == self.active_tab;
            self.tabs.remove(idx);
            let target_ix = if dup_ix > idx { dup_ix - 1 } else { dup_ix };
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len().saturating_sub(1);
            }
            if was_active {
                self.activate_tab(target_ix, window, cx);
            }
            self.record_recent_file(path, window, cx);
            self.persist_session_layout(cx);
            cx.notify();
            return;
        }
        let preserved_language_override = self.tabs[idx].meta().language_override.clone();
        let title = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("document")
            .to_string();
        let mut opened_successfully = false;

        match result {
            Ok(OpenOutcome::Editable {
                rope,
                identity,
                encoding,
                line_ending,
                readonly_reason,
                fingerprint,
            }) => {
                let incoming_bytes = rope.len() as u64;
                let slots = self.collect_hydrated_slots();
                match plan_hydration_admission(self.budget_config, &slots, incoming_bytes) {
                    AdmissionDecision::Admit {
                        dehydrate_tab_indices,
                    } => {
                        self.dehydrate_clean_tabs(&dehydrate_tab_indices, cx);
                        let meta = DocMeta {
                            title,
                            path: Some(path.clone()),
                            encoding,
                            line_ending,
                            origin_fingerprint: Some(fingerprint),
                            saved_identity: Some(identity),
                            readonly_reason,
                            paged: false,
                            saved_encoding: Some(encoding),
                            saved_line_ending: Some(line_ending),
                            language_override: preserved_language_override,
                        };
                        let tick = self.next_activation_tick();
                        let initial_prefs = self.current_view_prefs();
                        let mut adapter = EditorAdapter::new(
                            HydrateEditorParams {
                                doc_id,
                                lifecycle_gen,
                                meta,
                                rope,
                                initial_rev: Revision(0),
                                durable_rev: Revision(0),
                                is_clean_at_load: true,
                                disk_conflict: false,
                                initial_view,
                                initial_prefs,
                                activation_tick: tick,
                            },
                            window,
                            cx,
                        );
                        let sub = self.subscribe_editor(&adapter, cx);
                        adapter.attach_subscription(sub);
                        if idx == self.active_tab {
                            adapter.focus(window, cx);
                        }
                        self.tabs[idx] = TabSlot::Editable(adapter);
                        opened_successfully = true;
                    }
                    AdmissionDecision::OverBudget { reason, .. } => {
                        // Disk file over admission budget -> retain Read-Only Paged View (§15.2)!
                        match self.paging_service.open(doc_id, &path) {
                            Ok(handle) => {
                                let source_gen = handle.source_generation();
                                let meta = DocMeta {
                                    title,
                                    path: Some(path.clone()),
                                    encoding,
                                    line_ending,
                                    origin_fingerprint: Some(fingerprint),
                                    saved_identity: Some(identity),
                                    readonly_reason: Some(
                                        editor_services::ReadOnlyReason::LargeFile,
                                    ),
                                    paged: true,
                                    saved_encoding: Some(encoding),
                                    saved_line_ending: Some(line_ending),
                                    language_override: preserved_language_override,
                                };
                                let pv = PagedTabState::new(
                                    doc_id,
                                    lifecycle_gen,
                                    meta,
                                    handle,
                                    source_gen,
                                    editor_services::ReadOnlyReason::LargeFile,
                                    Some(format!("Paged Read-Only fallback — {reason}")),
                                    initial_view.scroll_byte,
                                );
                                self.tabs[idx] = TabSlot::Paged(pv);
                                self.ensure_paged_indexer_running(cx);
                                self.request_paged_viewport(
                                    idx,
                                    ViewportAnchor::ByteOffset(initial_view.scroll_byte),
                                    window,
                                    cx,
                                );
                                opened_successfully = true;
                            }
                            Err(err) => {
                                self.tabs[idx] = TabSlot::Failed {
                                    doc_id,
                                    lifecycle_gen,
                                    meta: DocMeta {
                                        title,
                                        path: Some(path.clone()),
                                        ..DocMeta::default()
                                    },
                                    error: err.message,
                                };
                            }
                        }
                    }
                }
            }
            Ok(OpenOutcome::Paged {
                doc,
                encoding,
                line_ending,
                readonly_reason,
                fingerprint,
                source_gen,
            }) => {
                let meta = DocMeta {
                    title,
                    path: Some(path.clone()),
                    encoding,
                    line_ending,
                    origin_fingerprint: Some(fingerprint),
                    saved_identity: None,
                    readonly_reason: Some(readonly_reason.clone()),
                    paged: true,
                    saved_encoding: Some(encoding),
                    saved_line_ending: Some(line_ending),
                    language_override: preserved_language_override,
                };
                let pv = PagedTabState::new(
                    doc_id,
                    lifecycle_gen,
                    meta,
                    doc,
                    source_gen,
                    readonly_reason,
                    None,
                    initial_view.scroll_byte,
                );
                self.tabs[idx] = TabSlot::Paged(pv);
                self.ensure_paged_indexer_running(cx);
                self.request_paged_viewport(
                    idx,
                    ViewportAnchor::ByteOffset(initial_view.scroll_byte),
                    window,
                    cx,
                );
                if idx == self.active_tab {
                    self.focus_handle.focus(window, cx);
                }
                opened_successfully = true;
            }
            Err(err) => {
                self.tabs[idx] = TabSlot::Failed {
                    doc_id,
                    lifecycle_gen,
                    meta: DocMeta {
                        title,
                        path: Some(path.clone()),
                        ..DocMeta::default()
                    },
                    error: err.message.clone(),
                };
                window.push_notification(
                    Notification::error(format!("Failed to open file: {}", err.message)),
                    cx,
                );
            }
        }

        if record_in_mru && opened_successfully {
            self.record_recent_file(path, window, cx);
        }

        self.persist_session_layout(cx);
        cx.notify();
    }

    pub fn request_paged_viewport(
        &mut self,
        tab_index: usize,
        anchor: ViewportAnchor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Paged(pv)) = self.tabs.get_mut(tab_index) else {
            return;
        };
        let doc_id = pv.doc_id;
        let handle = pv.handle.clone();
        let (seq, req, cancel) = pv.prepare_viewport_request(anchor);

        cx.spawn_in(window, async move |weak, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    let retry_deadline = Instant::now() + Duration::from_secs(15);
                    loop {
                        match handle.viewport(req.clone(), &cancel) {
                            Err(err)
                                if err.kind == ErrorKind::Busy
                                    && !cancel.is_cancelled()
                                    && Instant::now() < retry_deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(15));
                            }
                            other => break other,
                        }
                    }
                })
                .await;
            let _ = weak.update(cx, |this, cx| {
                let mut source_changed = false;
                if let Some(TabSlot::Paged(pv)) =
                    this.tabs.iter_mut().find(|s| s.doc_id() == doc_id)
                    && pv.apply_viewport_result(seq, res)
                {
                    source_changed = pv.source_error.is_some();
                }
                if source_changed && this.search.target_doc_id == Some(doc_id) {
                    this.search.cancel_active();
                    this.search.status = SearchStatus::Invalidated;
                }
                cx.notify();
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
        let has_incomplete = self.tabs.iter().any(|slot| {
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
                        if this.shutting_down || this.tabs.is_empty() {
                            this.paged_indexer_running = false;
                            return None;
                        }
                        let pick = (0..this.tabs.len())
                            .map(|offset| {
                                if offset == 0 {
                                    this.active_tab.min(this.tabs.len() - 1)
                                } else {
                                    let idx =
                                        (this.paged_index_rr_cursor + offset) % this.tabs.len();
                                    idx
                                }
                            })
                            .find(|&idx| {
                                matches!(
                                    &this.tabs[idx],
                                    TabSlot::Paged(pv)
                                        if !pv.is_loading
                                            && pv.source_error.is_none()
                                            && matches!(pv.index_status, IndexStatus::Indexing { .. })
                                )
                            });

                        if let Some(idx) = pick {
                            this.paged_index_rr_cursor = (idx + 1) % this.tabs.len();
                            if let TabSlot::Paged(pv) = &this.tabs[idx] {
                                return Some(Ok((
                                    pv.doc_id,
                                    pv.handle.clone(),
                                    pv.source_gen,
                                    pv.index_cancel_token(),
                                )));
                            }
                        }

                        // Check if any tab still needs indexing once its foreground viewport finishes.
                        let any_pending_after_viewport = this.tabs.iter().any(|slot| {
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
                    Some(Ok((doc_id, handle, source_gen, cancel))) => {
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
                                if let Some(TabSlot::Paged(pv)) =
                                    this.tabs.iter_mut().find(|s| s.doc_id() == doc_id)
                                {
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
                                            if this.search.target_doc_id == Some(doc_id) {
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

    fn flush_tab_commit(&mut self, index: usize) {
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(index) else {
            return;
        };
        let Some(commit) = ed.build_recovery_commit() else {
            return;
        };
        let Some(recovery) = &self.recovery else {
            return;
        };
        let target_rev = commit.new_rev;
        match recovery.try_commit(commit) {
            Ok(()) => ed.record_commit_queued(target_rev),
            Err(err) if err.kind == ErrorKind::Busy => ed.record_commit_busy(),
            Err(err) => {
                self.recovery_degraded_error = Some(err.message);
            }
        }
    }

    pub fn persist_session_layout(&mut self, cx: &App) {
        let Some(recovery) = &self.recovery else {
            self.layout_dirty = true;
            return;
        };
        let records: Vec<TabLayoutRecord> = self
            .tabs
            .iter()
            .filter(|slot| !self.closing_docs.contains_key(&slot.doc_id()))
            .map(|slot| slot.layout_record(cx))
            .collect();
        let active = self
            .tabs
            .get(self.active_tab)
            .map(TabSlot::doc_id)
            .filter(|id| !self.closing_docs.contains_key(id))
            .or_else(|| records.first().map(|r| r.doc_id));
        match recovery.save_session_layout(records, active) {
            Ok(()) => {
                self.layout_dirty = false;
            }
            Err(err) if err.kind == ErrorKind::Busy => {
                self.layout_dirty = true;
            }
            Err(err) => {
                self.layout_dirty = false;
                self.recovery_degraded_error = Some(err.message);
            }
        }
    }

    /// Saves an `Editable` tab to `explicit_path` (or its current `meta.path`, prompting if `None`).
    pub fn save_tab(
        &mut self,
        tab_index: usize,
        explicit_path: Option<PathBuf>,
        force_overwrite: bool,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.tabs.get(tab_index) else {
            return;
        };
        let TabSlot::Editable(ed) = slot else {
            window.push_notification(
                Notification::info(
                    "Paged or unhydrated tabs are read-only; nothing to save in place.",
                ),
                cx,
            );
            return;
        };

        if ed.is_readonly() && explicit_path.is_none() {
            window.push_notification(
                Notification::warning(
                    "Document is read-only (unsupported encoding or mixed EOL) to prevent silent corruption.",
                ),
                cx,
            );
            return;
        }

        let target_path = explicit_path.or_else(|| ed.meta.path.clone());
        let Some(target_path) = target_path else {
            self.prompt_save_as(tab_index, close_after_save, window, cx);
            return;
        };

        let is_same_origin = ed.meta.path.as_ref() == Some(&target_path);
        let captured_encoding = ed.meta.encoding;
        let captured_line_ending = ed.meta.line_ending;
        let captured_text_seq = ed.current_text_seq();
        let request = SaveRequest {
            doc_id: ed.doc_id,
            lifecycle_gen: ed.lifecycle_gen,
            captured_rev: ed.visible_revision,
            rope: ed.committed_rope().clone(),
            target_path: target_path.clone(),
            encoding: captured_encoding,
            line_ending: captured_line_ending,
            expected_fingerprint: if is_same_origin {
                ed.meta.origin_fingerprint.clone()
            } else {
                None
            },
            force_overwrite,
        };

        let recovery = self.recovery.clone();
        let faults = self.faults.clone();

        cx.spawn_in(window, async move |weak, cx| {
            let req_for_worker = request;
            let (outcome, mru_path) = cx
                .background_executor()
                .spawn(async move {
                    let out = save_document(&req_for_worker, recovery.as_ref(), &faults);
                    let normalized = std::fs::canonicalize(&req_for_worker.target_path)
                        .unwrap_or_else(|_| req_for_worker.target_path.clone());
                    (out, normalized)
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                this.handle_save_outcome(
                    outcome,
                    target_path,
                    mru_path,
                    captured_text_seq,
                    captured_encoding,
                    captured_line_ending,
                    close_after_save,
                    window,
                    cx,
                );
            });
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_save_outcome(
        &mut self,
        outcome: SaveOutcome,
        target_path: PathBuf,
        mru_path: PathBuf,
        captured_text_seq: u64,
        captured_encoding: EncodingSpec,
        captured_line_ending: LineEnding,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            SaveOutcome::Saved {
                doc_id,
                captured_rev,
                captured_identity,
                fingerprint,
            } => {
                self.complete_successful_save(
                    doc_id,
                    target_path,
                    mru_path,
                    captured_rev,
                    captured_text_seq,
                    captured_identity,
                    fingerprint,
                    captured_encoding,
                    captured_line_ending,
                    None,
                    close_after_save,
                    window,
                    cx,
                );
            }
            SaveOutcome::SavedWithRecoveryWarning {
                doc_id,
                captured_rev,
                captured_identity,
                fingerprint,
                warning,
            } => {
                self.complete_successful_save(
                    doc_id,
                    target_path,
                    mru_path,
                    captured_rev,
                    captured_text_seq,
                    captured_identity,
                    fingerprint,
                    captured_encoding,
                    captured_line_ending,
                    Some(warning.message),
                    close_after_save,
                    window,
                    cx,
                );
            }
            SaveOutcome::PublishedNotDurable { warning, .. } => {
                // Invariant: NEVER advance savepoint or clear dirty when durability sync failed!
                window.push_notification(
                    Notification::warning(format!(
                        "File published on disk, but durability sync was not confirmed: {}. Draft remains marked modified.",
                        warning.message
                    )),
                    cx,
                );
            }
            SaveOutcome::ConflictDetected { doc_id, .. } => {
                self.open_save_conflict_dialog(doc_id, target_path, close_after_save, window, cx);
            }
            SaveOutcome::Failed { error, .. } => {
                window.push_notification(
                    Notification::error(format!("Save failed: {}", error.message)),
                    cx,
                );
            }
        }
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn complete_successful_save(
        &mut self,
        doc_id: DocumentId,
        target_path: PathBuf,
        mru_path: PathBuf,
        captured_rev: Revision,
        captured_text_seq: u64,
        captured_identity: ContentIdentity,
        fingerprint: FileFingerprint,
        captured_encoding: EncodingSpec,
        captured_line_ending: LineEnding,
        recovery_warning: Option<String>,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(idx) = self.tabs.iter().position(|t| t.doc_id() == doc_id) else {
            return;
        };
        let mut check_req = None;
        if let TabSlot::Editable(ed) = &mut self.tabs[idx] {
            let check = ed.mark_saved(
                target_path.clone(),
                captured_rev,
                captured_text_seq,
                captured_identity,
                fingerprint,
                captured_encoding,
                captured_line_ending,
                window,
                cx,
            );
            if let Some(c) = check {
                check_req = Some((c, ed.committed_rope().clone()));
            }
        }

        if let Some((c, rope)) = check_req {
            self.spawn_savepoint_verification(
                doc_id,
                c.seq,
                c.checked_text_seq,
                c.saved_identity,
                rope,
                cx,
            );
        }

        self.flush_tab_commit(idx);
        self.persist_session_layout(cx);
        self.record_recent_file(mru_path, window, cx);

        let file_label = target_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");
        self.status_flash = Some((format!("Saved {file_label}"), Instant::now()));

        if let Some(warn) = recovery_warning {
            // F07: Separate file-saved fact from recovery-DB warning!
            window.push_notification(
                Notification::warning(format!(
                    "Saved '{file_label}' to disk, but recovery metadata update failed: {warn}"
                )),
                cx,
            );
        }

        if close_after_save
            && let Some(now_ix) = self.tabs.iter().position(|t| t.doc_id() == doc_id)
            && !self.tabs[now_ix].is_dirty()
        {
            self.close_tab_confirmed(now_ix, false, window, cx);
        }
    }

    fn open_save_conflict_dialog(
        &self,
        doc_id: DocumentId,
        target_path: PathBuf,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entity = cx.entity();
        let display = target_path.display().to_string();
        window.open_dialog(cx, move |dialog, _, _| {
            let entity_ok = entity.clone();
            let path_ok = target_path.clone();
            dialog
                .title("External File Change Detected")
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Overwrite Disk File")
                        .ok_variant(ButtonVariant::Danger)
                        .show_cancel(true)
                        .cancel_text("Keep Draft & Cancel")
                        .on_ok(move |_, window, cx| {
                            entity_ok.update(cx, |this, cx| {
                                if let Some(idx) =
                                    this.tabs.iter().position(|s| s.doc_id() == doc_id)
                                {
                                    this.save_tab(
                                        idx,
                                        Some(path_ok.clone()),
                                        true,
                                        close_after_save,
                                        window,
                                        cx,
                                    );
                                }
                            });
                            true
                        }),
                )
                .child(gpui_kit::div().text_sm().child(format!(
                    "'{display}' changed on disk after it was opened. Overwrite it with your current editor version, or cancel and use Save As…?"
                )))
        });
    }

    pub fn prompt_save_as(
        &mut self,
        tab_index: usize,
        close_after_save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.tabs.get(tab_index) else {
            return;
        };
        let doc_id = slot.doc_id();
        let initial_dir = slot
            .meta()
            .path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let suggested = format!("{}.txt", slot.title());
        let suggested_name = slot
            .meta()
            .path
            .as_deref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or(&suggested);

        let rx = cx.prompt_for_new_path(&initial_dir, Some(suggested_name));
        cx.spawn_in(window, async move |weak, cx| {
            if let Ok(Ok(Some(chosen_path))) = rx.await {
                let _ = weak.update_in(cx, |this, window, cx| {
                    if let Some(idx) = this.tabs.iter().position(|t| t.doc_id() == doc_id) {
                        // macOS NSSavePanel already prompted the user to confirm replacing an existing path.
                        this.save_tab(idx, Some(chosen_path), true, close_after_save, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    /// Exports an `OverBudgetDraft` directly from SQLite to a user-selected file on a background
    /// worker without creating an `EditorState` (`DesignReview` Correction 2).
    pub fn export_over_budget_draft(
        &mut self,
        doc_id: DocumentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(recovery) = self.recovery.clone() else {
            window.push_notification(
                Notification::error("Recovery service unavailable; cannot export draft."),
                cx,
            );
            return;
        };
        let dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some("recovered-draft.txt"));
        let faults = self.faults.clone();

        cx.spawn_in(window, async move |weak, cx| {
            let Ok(Ok(Some(dest_path))) = rx.await else {
                return;
            };
            let res = cx
                .background_executor()
                .spawn(async move {
                    let draft = recovery
                        .request_hydrate(doc_id)?
                        .recv_timeout(Duration::from_secs(20))
                        .map_err(|e| ServiceError::new(ErrorKind::Storage, e.to_string()))??;
                    let req = SaveRequest {
                        doc_id: draft.doc_id,
                        lifecycle_gen: draft.lifecycle_gen,
                        captured_rev: draft.durable_rev,
                        rope: draft.rope,
                        target_path: dest_path.clone(),
                        encoding: if draft.meta.encoding == EncodingSpec::UnsupportedReadOnly {
                            EncodingSpec::Utf8
                        } else {
                            draft.meta.encoding
                        },
                        line_ending: if draft.meta.line_ending == LineEnding::MixedReadOnly {
                            LineEnding::Lf
                        } else {
                            draft.meta.line_ending
                        },
                        expected_fingerprint: None,
                        force_overwrite: true,
                    };
                    Ok::<(PathBuf, SaveOutcome), ServiceError>((
                        dest_path,
                        save_document(&req, Some(&recovery), &faults),
                    ))
                })
                .await;

            let _ = weak.update_in(cx, |_this, window, cx| match res {
                Ok((
                    path,
                    SaveOutcome::Saved { .. } | SaveOutcome::SavedWithRecoveryWarning { .. },
                )) => {
                    window.push_notification(
                        Notification::success(format!(
                            "Exported draft safely to {}",
                            path.display()
                        )),
                        cx,
                    );
                }
                Ok((_, other)) => {
                    window.push_notification(
                        Notification::warning(format!("Draft export incomplete: {other:?}")),
                        cx,
                    );
                }
                Err(err) => {
                    window.push_notification(
                        Notification::error(format!("Draft export failed: {}", err.message)),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub fn request_close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        if self.closing_docs.contains_key(&self.tabs[index].doc_id()) {
            return;
        }
        if !self.tabs[index].is_dirty() {
            self.close_tab_confirmed(index, false, window, cx);
            return;
        }

        let doc_id = self.tabs[index].doc_id();
        let title = self.tabs[index].title().to_string();
        let is_hydrating_or_over_budget = !matches!(self.tabs[index], TabSlot::Editable(_));
        let entity = cx.entity();

        window.open_dialog(cx, move |dialog, _, _| {
            let e_save = entity.clone();
            let e_discard = entity.clone();
            dialog
                .title(format!("Unsaved Changes in \"{title}\""))
                .footer(
                    gpui_kit::component::h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            gpui_kit::component::button::Button::new("cancel-close")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            gpui_kit::component::button::Button::new("discard-close")
                                .danger()
                                .label("Discard Draft")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    e_discard.update(cx, |this, cx| {
                                        if let Some(ix) =
                                            this.tabs.iter().position(|t| t.doc_id() == doc_id)
                                        {
                                            this.close_tab_confirmed(ix, true, window, cx);
                                        }
                                    });
                                }),
                        )
                        .child(
                            gpui_kit::component::button::Button::new("save-close")
                                .primary()
                                .label(if is_hydrating_over(is_hydrating_or_over_budget) {
                                    "Export Draft…"
                                } else {
                                    "Save & Close"
                                })
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    e_save.update(cx, |this, cx| {
                                        if let Some(ix) =
                                            this.tabs.iter().position(|t| t.doc_id() == doc_id)
                                        {
                                            if is_hydrating_or_over_budget {
                                                this.export_over_budget_draft(doc_id, window, cx);
                                            } else {
                                                this.save_tab(ix, None, false, true, window, cx);
                                            }
                                        }
                                    });
                                }),
                        ),
                )
                .child(gpui_kit::div().text_sm().child(
                    "Save your changes to disk before closing, or permanently discard this draft?",
                ))
        });
    }

    /// Closes a tab safely (`StorageAdversarial` close barrier):
    /// retains the tab in `self.tabs` until `RecoveryHandle::close_document` is durably
    /// acknowledged (retrying `ErrorKind::Busy` asynchronously), and updates `windows_views`
    /// layout ONLY AFTER `close_document` succeeds. On storage failure, preserves the tab
    /// and notifies the user without data loss.
    pub fn close_tab_confirmed(
        &mut self,
        index: usize,
        discard_draft: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if index >= self.tabs.len() {
            return;
        }
        let doc_id = self.tabs[index].doc_id();
        if self.closing_docs.contains_key(&doc_id) {
            return;
        }
        self.closing_docs.insert(doc_id, discard_draft);
        let prev_gen = self.tabs[index].lifecycle_gen();
        // R06: Explicitly discarding bumps LifecycleGen so late snapshots/edits can never resurrect it!
        let close_gen = close_lifecycle_gen(prev_gen, discard_draft);

        let Some(recovery) = self.recovery.clone() else {
            self.closing_docs.remove(&doc_id);
            self.finalize_tab_removal_after_close_ack(doc_id, prev_gen, discard_draft, window, cx);
            return;
        };

        let pending_commit = if !discard_draft {
            if let TabSlot::Editable(ed) = &mut self.tabs[index] {
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
                ed.build_recovery_commit()
            } else {
                None
            }
        } else {
            if let TabSlot::Editable(ed) = &mut self.tabs[index] {
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
            }
            None
        };

        cx.spawn_in(window, async move |weak, cx| {
            let close_res = cx
                .background_executor()
                .spawn(async move {
                    if let Some(mut commit) = pending_commit {
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

                    let deadline = Instant::now() + Duration::from_secs(5);
                    let rx = loop {
                        match recovery.close_document(doc_id, close_gen, discard_draft) {
                            Ok(rx) => break rx,
                            Err(e)
                                if e.kind == ErrorKind::Busy
                                    && Instant::now() < deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            Err(e) => return Err(e),
                        }
                    };
                    rx.recv_timeout(Duration::from_secs(10)).map_err(|e| {
                        ServiceError::new(ErrorKind::Storage, format!("close_document timeout: {e}"))
                    })?
                })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                if this.shutting_down {
                    return;
                }
                this.closing_docs.remove(&doc_id);
                match close_res {
                    Ok(()) => {
                        this.finalize_tab_removal_after_close_ack(
                            doc_id,
                            prev_gen,
                            discard_draft,
                            window,
                            cx,
                        );
                    }
                    Err(err) => {
                        if let Some(TabSlot::Editable(ed)) =
                            this.tabs.iter_mut().find(|t| t.doc_id() == doc_id)
                            && !ed.is_readonly()
                        {
                            ed.editor().update(cx, |s, cx| s.set_readonly(false, cx));
                        }
                        this.persist_session_layout(cx);
                        window.push_notification(
                            Notification::error(format!(
                                "Could not persist tab close ({}); tab retained to prevent data loss.",
                                err.message
                            )),
                            cx,
                        );
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn finalize_tab_removal_after_close_ack(
        &mut self,
        doc_id: DocumentId,
        prev_gen: LifecycleGen,
        discard_draft: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|t| t.doc_id() == doc_id) else {
            return;
        };
        let removed = self.tabs.remove(index);

        if !discard_draft && (removed.meta().path.is_some() || removed.is_dirty()) {
            let record = removed.layout_record(cx);
            self.recently_closed.insert(
                0,
                SessionTabStub {
                    doc_id,
                    lifecycle_gen: prev_gen,
                    durable_rev: match &removed {
                        TabSlot::Editable(ed) => ed.durable_revision,
                        TabSlot::Dehydrated(s)
                        | TabSlot::Hydrating { stub: s, .. }
                        | TabSlot::OverBudgetDraft { stub: s, .. } => s.durable_rev,
                        _ => Revision(0),
                    },
                    meta: record.meta,
                    view: record.view,
                    has_checkpoint: removed.is_dirty(),
                },
            );
            if self.recently_closed.len() > 32 {
                self.recently_closed.pop();
            }
        }

        if self.search.target_doc_id == Some(doc_id) {
            self.search.clear_results();
        }

        if self.tabs.is_empty() {
            self.create_untitled_tab(true, window, cx);
        } else {
            let next_ix = index.min(self.tabs.len() - 1);
            self.activate_tab(next_ix, window, cx);
        }
        self.persist_session_layout(cx);
        cx.notify();
    }

    pub fn reopen_recently_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(stub) = self.recently_closed.first().cloned() else {
            self.status_flash = Some(("No recently closed tabs".to_string(), Instant::now()));
            cx.notify();
            return;
        };
        self.recently_closed.remove(0);

        let new_ix = self.tabs.len();
        self.tabs.push(TabSlot::Dehydrated(stub));
        self.activate_tab(new_ix, window, cx);
    }

    /// Executes the clean shutdown flush barrier (`§8.2`, `§8.5`, `R01`, `R05`).
    pub fn initiate_clean_quit(
        &mut self,
        force_without_recovery: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.search.cancel_active();

        let has_any_dirty = self
            .tabs
            .iter()
            .any(|slot| slot.is_dirty() && self.closing_docs.get(&slot.doc_id()) != Some(&true));
        let has_oversized_dirty = self.tabs.iter().any(|slot| match slot {
            TabSlot::Editable(ed) => {
                self.closing_docs.get(&ed.doc_id) != Some(&true)
                    && ed.is_dirty()
                    && ed.committed_byte_len() > MAX_ORDINARY_DOC_BYTES as u64
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
        // Capture only unpersisted commits (`U-B5`: never re-queue already-durable 50 MiB drafts)
        // and exclude closing/discarded tabs (`U-B4`: never resurrect closed/discarded tabs).
        let mut final_commits = Vec::new();
        for slot in &mut self.tabs {
            if let TabSlot::Editable(ed) = slot {
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
                if should_enqueue_quit_commit(
                    self.closing_docs.contains_key(&ed.doc_id),
                    ed.is_dirty(),
                    ed.visible_revision,
                    ed.durable_revision,
                    ed.committed_byte_len(),
                ) {
                    let commit = ed.build_recovery_commit().unwrap_or_else(|| {
                        editor_services::RecoveryCommit {
                            doc_id: ed.doc_id,
                            lifecycle_gen: ed.lifecycle_gen,
                            base_rev: ed.queued_revision,
                            new_rev: ed.visible_revision.max(Revision(1)),
                            rope: ed.committed_rope().clone(),
                            deltas: Vec::new(),
                            meta: ed.meta.clone(),
                        }
                    });
                    final_commits.push(commit);
                }
            }
        }
        let pending_closes: Vec<(DocumentId, LifecycleGen, bool)> = self
            .closing_docs
            .iter()
            .filter_map(|(&id, &discard)| {
                let prev_gen = self.tabs.iter().find(|s| s.doc_id() == id)?.lifecycle_gen();
                let close_gen = close_lifecycle_gen(prev_gen, discard);
                Some((id, close_gen, discard))
            })
            .collect();
        let layout_records: Vec<TabLayoutRecord> = self
            .tabs
            .iter()
            .filter(|s| !self.closing_docs.contains_key(&s.doc_id()))
            .map(|s| s.layout_record(cx))
            .collect();
        let active_doc = self
            .tabs
            .get(self.active_tab)
            .map(TabSlot::doc_id)
            .filter(|id| !self.closing_docs.contains_key(id))
            .or_else(|| layout_records.first().map(|r| r.doc_id));

        let smoke_outcome = self.smoke_outcome.clone();
        let data_dir = self.cli.data_dir.clone();
        let smoke_report_path = self.cli.smoke_report.clone();
        let process_start = self.process_start;

        cx.spawn_in(window, async move |weak, cx| {
            let (barrier_res, shell_res, finalized_smoke_report) = cx
                .background_executor()
                .spawn(async move {
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
                        for (doc_id, close_gen, discard) in pending_closes {
                            let deadline = Instant::now() + Duration::from_secs(5);
                            let maybe_rx = loop {
                                match recovery.close_document(doc_id, close_gen, discard) {
                                    Ok(rx) => break Some(rx),
                                    Err(e)
                                        if e.kind == ErrorKind::Busy
                                            && Instant::now() < deadline =>
                                    {
                                        std::thread::sleep(Duration::from_millis(20));
                                    }
                                    Err(e) if e.kind == ErrorKind::Stale => break None,
                                    Err(e) => return Err(e),
                                }
                            };
                            let Some(rx) = maybe_rx else {
                                continue;
                            };
                            match rx.recv_timeout(Duration::from_secs(5)) {
                                Ok(Ok(())) => {}
                                Ok(Err(e)) if e.kind == ErrorKind::Stale => {}
                                Ok(Err(e)) => return Err(e),
                                Err(recv_err) => {
                                    return Err(ServiceError::new(
                                        ErrorKind::Storage,
                                        format!(
                                            "quit close_document acknowledgement failed: {recv_err}"
                                        ),
                                    ));
                                }
                            }
                        }
                        let layout_deadline = Instant::now() + Duration::from_secs(5);
                        loop {
                            match recovery.save_session_layout(layout_records.clone(), active_doc) {
                                Ok(()) => break,
                                Err(e)
                                    if e.kind == ErrorKind::Busy
                                        && Instant::now() < layout_deadline =>
                                {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                Err(e) => return Err(e),
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
                        rx.recv_timeout(Duration::from_secs(10)).map_err(|e| {
                            ServiceError::new(
                                ErrorKind::Storage,
                                format!("shutdown flush timeout: {e}"),
                            )
                        })?
                    })();

                    // Guard 2: Drop primary `RecoveryHandle` BEFORE opening any verification DB handle.
                    drop(recovery);

                    // Flush/shutdown UI shell preferences worker separately so preference I/O
                    // errors never masquerade as draft-recovery failures or block rescue save!
                    let should_shutdown_shell =
                        primary_res.is_ok() || !has_any_dirty || force_without_recovery;
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
                                report.shutdown_durable_ack_observed = true;
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

                    (primary_res, shell_res, smoke_report)
                })
                .await;

            if let Some(report) = finalized_smoke_report {
                // Guard 3: Emit report + flush stdout/stderr BEFORE `[NSApp terminate:nil]` (`cx.quit`)
                // and exit `1` on smoke failure so macOS AppKit never swallows failures or buffers.
                crate::smoke::emit_smoke_report(&report, smoke_report_path.as_deref());
                if report.passed {
                    if weak
                        .update_in(cx, |this, _, cx| {
                            this.shutdown_durable_ack.store(true, Ordering::Release);
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
                    this.shutdown_durable_ack.store(true, Ordering::Release);
                    this.quit_confirmed = true;
                    cx.quit();
                }
                Err(err) if has_any_dirty && !force_without_recovery => {
                    this.shutting_down = false;
                    if let Err(shell_err) = shell_res {
                        this.shell_persistence_warning = Some(shell_err.message);
                    }
                    for slot in &mut this.tabs {
                        if let TabSlot::Editable(ed) = slot
                            && !ed.is_readonly()
                        {
                            ed.editor().update(cx, |s, cx| s.set_readonly(false, cx));
                        }
                    }
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
        self.search.visible = true;
        self.search.replace_visible = with_replace;
        self.goto_visible = false;
        self.palette_visible = false;
        self.palette_prior_focus = None;

        // Seed search box from active selection if non-empty and short.
        if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
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
        self.search.visible = false;
        self.search.cancel_active();
        self.goto_visible = false;
        self.palette_visible = false;
        self.palette_prior_focus = None;
        if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
            ed.clear_search_highlights(cx);
            ed.focus(window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
        cx.notify();
    }

    pub fn run_search_from_inputs(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let query = self.search_query_input.read(cx).value().to_string();
        let Some(slot) = self.tabs.get(self.active_tab) else {
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
                            &query,
                            SearchTarget::EditableSnapshot { doc_id, rev, rope },
                        );
                        self.rebuild_search_item_sizes();
                    }
                    SearchQueryKind::CaseInsensitive | SearchQueryKind::Regex => {
                        let kind = self.search.query_kind;
                        let (qid, cancel) = self
                            .search
                            .begin_background_extended_search(doc_id, rev, &query);
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
                                let current_doc_rev = this.tabs.iter().find_map(|s| match s {
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
                self.search
                    .start_streaming_literal(&self.search_service, &query, target);
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

        match self.tabs.get_mut(self.active_tab) {
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
        let Some(hit) = self.search.matches.get(match_index).cloned() else {
            return;
        };
        self.search.selected_index = Some(match_index);
        self.sync_active_search_decorations(cx);

        match self.tabs.get_mut(self.active_tab) {
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
                self.request_paged_viewport(
                    self.active_tab,
                    ViewportAnchor::ByteOffset(target_offset),
                    window,
                    cx,
                );
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
        let query = self.search_query_input.read(cx).value().to_string();
        let replacement = self.search_replace_input.read(cx).value().to_string();

        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(self.active_tab) else {
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
            && self.search.target_doc_id == Some(doc_id)
            && self.search.target_editable_rev == Some(current_rev)
            && self.search.last_query_text == query;

        if matches_fresh {
            self.apply_planned_replace_on_active(
                doc_id,
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
                .begin_background_extended_search(doc_id, current_rev, &query);
        self.rebuild_search_item_sizes();
        cx.notify();

        cx.spawn_in(window, async move |weak, cx| {
            let q_worker = query.clone();
            let res = cx
                .background_executor()
                .spawn(async move { run_editable_extended_search(&rope, &q_worker, kind, &cancel) })
                .await;

            let _ = weak.update_in(cx, |this, window, cx| {
                let live_rev = this.tabs.iter().find_map(|s| match s {
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
                    doc_id,
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
        doc_id: DocumentId,
        base_rev: Revision,
        query: &str,
        replacement: &str,
        single: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if ed.doc_id != doc_id {
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
        match self.apply_tab_atomic_edits(self.active_tab, base_rev, &edits, window, cx) {
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
        let raw = self.goto_input.read(cx).value().to_string();
        let is_paged = matches!(self.tabs.get(self.active_tab), Some(TabSlot::Paged(_)));

        match parse_jump_query(&raw, is_paged) {
            Ok(anchor) => {
                self.goto_visible = false;
                match self.tabs.get(self.active_tab) {
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
                        self.request_paged_viewport(self.active_tab, anchor, window, cx);
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
                    for path in paths {
                        this.open_path(path, window, cx);
                    }
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
        self.save_tab(self.active_tab, None, false, false, window, cx);
    }

    pub(crate) fn on_save_file_as(
        &mut self,
        _: &SaveFileAs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_save_as(self.active_tab, false, window, cx);
    }

    pub(crate) fn on_close_active_tab(
        &mut self,
        _: &CloseActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_close_tab(self.active_tab, window, cx);
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
        if !self.tabs.is_empty() {
            let next = (self.active_tab + 1) % self.tabs.len();
            self.activate_tab(next, window, cx);
        }
    }

    pub(crate) fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            let prev = if self.active_tab == 0 {
                self.tabs.len() - 1
            } else {
                self.active_tab - 1
            };
            self.activate_tab(prev, window, cx);
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
        self.palette_visible = false;
        self.palette_prior_focus = None;
        self.goto_visible = !self.goto_visible;
        if self.goto_visible {
            self.goto_input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        } else if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
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
        let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) else {
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
        let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) else {
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
        let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) else {
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
        self.request_reload_active(false, window, cx);
    }

    /// Reloads the active file-backed tab from disk (`U-B6` / `U-B6b`):
    /// - Works cleanly on `disk_conflict` banners, confirming discard only when `has_local_edits()`.
    /// - When `RecoveryHandle` is active on a non-paged tab, guards `old_doc_id` in `closing_docs`
    ///   (`readonly = true`), durably tombstones `old_doc_id` (`state = 2`), and replaces the tab
    ///   slot in-place with a **fresh** `DocumentId` at `LifecycleGen(1)` so subsequent commits
    ///   and session layouts are never rejected by `old_doc_id`'s permanent tombstone.
    pub fn request_reload_active(
        &mut self,
        confirm_discard_local_edits: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.tabs.get(self.active_tab) else {
            return;
        };
        let old_doc_id = slot.doc_id();
        if self.closing_docs.contains_key(&old_doc_id) {
            return;
        }
        let Some(path) = slot.meta().path.clone() else {
            return;
        };
        let prev_gen = slot.lifecycle_gen();
        let preserved_lang_override = slot.meta().language_override.clone();
        let view = slot.layout_record(cx).view;
        let is_paged_or_failed = matches!(slot, TabSlot::Paged(_) | TabSlot::Failed { .. });
        let has_local_edits = match slot {
            TabSlot::Editable(ed) => ed.has_local_edits(),
            _ => slot.is_dirty(),
        };

        if has_local_edits && !confirm_discard_local_edits {
            let title = slot.title().to_string();
            let entity = cx.entity();
            window.open_dialog(cx, move |dialog, _, _| {
                let e_reload = entity.clone();
                dialog
                    .title(format!("Discard Local Edits in \"{title}\"?"))
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("Discard & Reload from Disk")
                            .ok_variant(ButtonVariant::Danger)
                            .show_cancel(true)
                            .cancel_text("Cancel")
                            .on_ok(move |_, window, cx| {
                                e_reload.update(cx, |this, cx| {
                                    if let Some(ix) =
                                        this.tabs.iter().position(|t| t.doc_id() == old_doc_id)
                                    {
                                        this.active_tab = ix;
                                        this.request_reload_active(true, window, cx);
                                    }
                                });
                                true
                            }),
                    )
                    .child(gpui_kit::div().text_sm().child(
                        "Reloading from disk will discard your unsaved editor changes and load the current disk file.",
                    ))
            });
            return;
        }

        if !is_paged_or_failed && let Some(recovery) = self.recovery.clone() {
            self.closing_docs.insert(old_doc_id, true);
            if let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(self.active_tab) {
                ed.editor().update(cx, |s, cx| s.set_readonly(true, cx));
            }
            let close_gen = close_lifecycle_gen(prev_gen, true);

            cx.spawn_in(window, async move |weak, cx| {
                let discard_res = cx
                    .background_executor()
                    .spawn(async move {
                        let deadline = Instant::now() + Duration::from_secs(5);
                        let rx = loop {
                            match recovery.close_document(old_doc_id, close_gen, true) {
                                Ok(rx) => break rx,
                                Err(e)
                                    if e.kind == ErrorKind::Busy && Instant::now() < deadline =>
                                {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                Err(e) => return Err(e),
                            }
                        };
                        rx.recv_timeout(Duration::from_secs(10)).map_err(|e| {
                            ServiceError::new(
                                ErrorKind::Storage,
                                format!("reload discard timeout: {e}"),
                            )
                        })?
                    })
                    .await;

                let _ = weak.update_in(cx, |this, window, cx| {
                    if this.shutting_down {
                        return;
                    }
                    this.closing_docs.remove(&old_doc_id);
                    match discard_res {
                        Ok(()) => {
                            let Some(slot_ix) =
                                this.tabs.iter().position(|t| t.doc_id() == old_doc_id)
                            else {
                                return;
                            };
                            this.recently_closed.retain(|s| s.doc_id != old_doc_id);
                            if this.search.target_doc_id == Some(old_doc_id) {
                                this.search.clear_results();
                                this.rebuild_search_item_sizes();
                            }

                            let (fresh_doc_id, fresh_gen) = fresh_reloaded_identity(old_doc_id);
                            this.hydrate_req_counter = this.hydrate_req_counter.wrapping_add(1);
                            let req_seq = this.hydrate_req_counter;
                            let title = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("document")
                                .to_string();
                            let stub = SessionTabStub {
                                doc_id: fresh_doc_id,
                                lifecycle_gen: fresh_gen,
                                durable_rev: Revision(0),
                                meta: DocMeta {
                                    title,
                                    path: Some(path.clone()),
                                    language_override: preserved_lang_override,
                                    ..DocMeta::default()
                                },
                                view: view.clone(),
                                has_checkpoint: false,
                            };
                            this.tabs[slot_ix] = TabSlot::Hydrating { stub, req_seq };
                            this.persist_session_layout(cx);
                            this.spawn_disk_open_into_slot(
                                fresh_doc_id,
                                fresh_gen,
                                Some(view),
                                path,
                                false,
                                window,
                                cx,
                            );
                            cx.notify();
                        }
                        Err(err) => {
                            if let Some(TabSlot::Editable(ed)) =
                                this.tabs.iter_mut().find(|t| t.doc_id() == old_doc_id)
                                && !ed.is_readonly()
                            {
                                ed.editor().update(cx, |s, cx| s.set_readonly(false, cx));
                            }
                            this.persist_session_layout(cx);
                            window.push_notification(
                                Notification::error(format!(
                                    "Could not discard old draft before reload: {}",
                                    err.message
                                )),
                                cx,
                            );
                            cx.notify();
                        }
                    }
                });
            })
            .detach();
        } else {
            self.spawn_disk_open_into_slot(
                old_doc_id,
                prev_gen,
                Some(view),
                path,
                false,
                window,
                cx,
            );
        }
    }

    pub(crate) fn on_external_drop(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for path in paths.paths() {
            self.open_path(path.clone(), window, cx);
        }
    }

    pub(crate) fn on_quit(&mut self, _: &QuitEditor, window: &mut Window, cx: &mut Context<Self>) {
        self.initiate_clean_quit(false, window, cx);
    }

    pub(crate) fn copy_paged_slice(&mut self, cx: &mut Context<Self>) {
        if let Some(TabSlot::Paged(pv)) = self.tabs.get(self.active_tab)
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

    // --- Smoke & Programmatic Verification Hooks ---

    pub fn active_editable_mut(&mut self) -> Option<&mut EditorAdapter> {
        match self.tabs.get_mut(self.active_tab) {
            Some(TabSlot::Editable(ed)) => Some(ed),
            _ => None,
        }
    }

    pub fn apply_tab_atomic_edits(
        &mut self,
        slot_ix: usize,
        expected_base_rev: Revision,
        edits: &[(std::ops::Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Revision, ApplyEditsError> {
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) else {
            return Err(ApplyEditsError::ReadOnly);
        };
        let (new_rev, maybe_outcome) = ed.apply_edits(expected_base_rev, edits, window, cx)?;
        if let Some(outcome) = maybe_outcome {
            self.finish_committed_mutation(slot_ix, outcome, cx);
        }
        Ok(new_rev)
    }

    pub fn apply_active_atomic_edits(
        &mut self,
        expected_base_rev: Revision,
        edits: &[(std::ops::Range<usize>, String)],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Revision, ApplyEditsError> {
        self.apply_tab_atomic_edits(self.active_tab, expected_base_rev, edits, window, cx)
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
        if let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(self.active_tab) {
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
        let slot_ix = self.active_tab;
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) else {
            return;
        };
        match ed.set_line_ending(line_ending) {
            Ok(outcome) => self.finish_format_change(slot_ix, outcome, cx),
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
        let slot_ix = self.active_tab;
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) else {
            return;
        };
        match ed.set_encoding(encoding) {
            Ok(outcome) => self.finish_format_change(slot_ix, outcome, cx),
            Err(msg) => {
                window.push_notification(Notification::warning(msg), cx);
            }
        }
    }

    fn finish_format_change(
        &mut self,
        slot_ix: usize,
        outcome: FormatChangeOutcome,
        cx: &mut Context<Self>,
    ) {
        match outcome {
            FormatChangeOutcome::NoOp => {}
            FormatChangeOutcome::MetadataOnlyUntitled => {
                self.persist_session_layout(cx);
                cx.notify();
            }
            FormatChangeOutcome::RecoveryCommitRequired { new_rev } => {
                let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) else {
                    return;
                };
                let doc_id = ed.doc_id;
                let immediate_commit = if ed.durable_revision.0 == 0 {
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
                    if let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(slot_ix) {
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
        let Some(TabSlot::Editable(ed)) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if ed.set_language_override(language_id, window, cx) {
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
        if !self.palette_visible {
            self.palette_prior_focus = window.focused(cx);
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
        let prior = self.palette_prior_focus.take();
        self.restore_focus_after_overlay(prior, window, cx);
        cx.notify();
    }

    fn restore_focus_after_overlay(
        &self,
        prior: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prior = prior.filter(|handle| {
            self.search.visible
                && (*handle == self.search_query_input.read(cx).focus_handle(cx)
                    || (self.search.replace_visible
                        && *handle == self.search_replace_input.read(cx).focus_handle(cx)))
        });
        if let Some(handle) = prior {
            handle.focus(window, cx);
        } else if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
            ed.focus(window, cx);
        } else {
            self.focus_handle.focus(window, cx);
        }
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

        // Close overlay and focus active EditorState BEFORE dispatching target command
        // so editor actions (`Undo`, `Redo`, `SelectAll`, `Indent`, `Outdent`, line ops)
        // NEVER hit the palette search input focus trap!
        self.palette_visible = false;
        let prior = self.palette_prior_focus.take();
        if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
            ed.focus(window, cx);
        } else {
            self.restore_focus_after_overlay(prior, window, cx);
        }

        match target {
            PaletteCommandTarget::ActivateOpenTab(ix) => {
                self.activate_tab(ix, window, cx);
            }
            PaletteCommandTarget::OpenRecentPath(path) => {
                self.open_path(path, window, cx);
            }
            PaletteCommandTarget::SwitchToFileMode => {}
            PaletteCommandTarget::NewTab => {
                self.create_untitled_tab(true, window, cx);
            }
            PaletteCommandTarget::OpenFileDialog => {
                self.on_open_file(&OpenFile, window, cx);
            }
            PaletteCommandTarget::SaveFile => {
                self.save_tab(self.active_tab, None, false, false, window, cx);
            }
            PaletteCommandTarget::SaveFileAs => {
                self.prompt_save_as(self.active_tab, false, window, cx);
            }
            PaletteCommandTarget::ReloadFromDisk => {
                self.request_reload_active(false, window, cx);
            }
            PaletteCommandTarget::ReopenClosedTab => {
                self.reopen_recently_closed(window, cx);
            }
            PaletteCommandTarget::CloseActiveTab => {
                self.request_close_tab(self.active_tab, window, cx);
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
                if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
                    ed.undo(window, cx);
                }
            }
            PaletteCommandTarget::EditRedo => {
                if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
                    ed.redo(window, cx);
                }
            }
            PaletteCommandTarget::EditSelectAll => {
                if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
                    ed.focus(window, cx);
                    window.dispatch_action(Box::new(SelectAll), cx);
                }
            }
            PaletteCommandTarget::IndentSelection => {
                if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
                    ed.focus(window, cx);
                    window.dispatch_action(Box::new(Indent), cx);
                }
            }
            PaletteCommandTarget::OutdentSelection => {
                if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
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
        let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) else {
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
            }) => {
                match self.apply_tab_atomic_edits(self.active_tab, base_rev, &edits, window, cx) {
                    Ok(_) => {
                        if let Some(TabSlot::Editable(ed)) = self.tabs.get(self.active_tab) {
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
                }
            }
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

fn this_persist_layout_if_clean(this: &mut EditorWorkspace, cx: &App) {
    this.persist_session_layout(cx);
}

#[inline]
fn is_hydrating_over(flag: bool) -> bool {
    flag
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
#[inline]
pub(crate) fn close_lifecycle_gen(prev_gen: LifecycleGen, discard_draft: bool) -> LifecycleGen {
    if discard_draft {
        LifecycleGen(prev_gen.0.saturating_add(1))
    } else {
        prev_gen
    }
}

#[inline]
pub(crate) fn fresh_reloaded_identity(old_doc_id: DocumentId) -> (DocumentId, LifecycleGen) {
    let mut fresh = DocumentId::new();
    while fresh == old_doc_id {
        fresh = DocumentId::new();
    }
    (fresh, LifecycleGen(1))
}

#[inline]
pub(crate) fn should_enqueue_quit_commit(
    is_closing: bool,
    is_dirty: bool,
    visible_rev: Revision,
    durable_rev: Revision,
    byte_len: u64,
) -> bool {
    if is_closing || byte_len > MAX_ORDINARY_DOC_BYTES as u64 {
        return false;
    }
    visible_rev > durable_rev || (is_dirty && durable_rev.0 == 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StubHydrationSource {
    DiskOpen,
    EmptyCleanUntitled,
    RecoveryDatabase,
}

#[inline]
pub fn classify_stub_hydration_source(has_checkpoint: bool, has_path: bool) -> StubHydrationSource {
    if has_checkpoint {
        StubHydrationSource::RecoveryDatabase
    } else if has_path {
        StubHydrationSource::DiskOpen
    } else {
        StubHydrationSource::EmptyCleanUntitled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quit_commit_predicate_skips_closing_already_durable_and_oversized_buffers() {
        // Already-durable 50 MiB dirty draft (`durable_rev == visible_rev == 5`) -> DO NOT re-queue!
        assert!(!should_enqueue_quit_commit(
            false,
            true,
            Revision(5),
            Revision(5),
            50 * 1024 * 1024,
        ));

        // Unpersisted edit (`visible_rev 6 > durable_rev 5`) within 64 MiB -> DO enqueue!
        assert!(should_enqueue_quit_commit(
            false,
            true,
            Revision(6),
            Revision(5),
            50 * 1024 * 1024,
        ));

        // Closing/discarding tab (`is_closing == true`) -> NEVER enqueue!
        assert!(!should_enqueue_quit_commit(
            true,
            true,
            Revision(6),
            Revision(5),
            1024,
        ));

        // > 64 MiB buffer -> blocked by degraded-quit dialog, never pushed into RecoveryService!
        assert!(!should_enqueue_quit_commit(
            false,
            true,
            Revision(2),
            Revision(0),
            (MAX_ORDINARY_DOC_BYTES as u64) + 1,
        ));
    }

    #[test]
    fn close_lifecycle_gen_advances_only_on_discard() {
        assert_eq!(close_lifecycle_gen(LifecycleGen(3), false), LifecycleGen(3));
        assert_eq!(close_lifecycle_gen(LifecycleGen(3), true), LifecycleGen(4));
    }

    #[test]
    fn clean_untitled_stub_reconstitutes_without_recovery_db_query() {
        assert_eq!(
            classify_stub_hydration_source(false, false),
            StubHydrationSource::EmptyCleanUntitled
        );
        assert_eq!(
            classify_stub_hydration_source(false, true),
            StubHydrationSource::DiskOpen
        );
        assert_eq!(
            classify_stub_hydration_source(true, false),
            StubHydrationSource::RecoveryDatabase
        );
        assert_eq!(
            classify_stub_hydration_source(true, true),
            StubHydrationSource::RecoveryDatabase
        );
    }
}
