use editor_services::{
    ContentIdentity, DocMeta, DocumentId, EncodingSpec, FileFingerprint, LifecycleGen, LineEnding,
    RecoveryCommit, Revision, TextDelta, ViewState,
};
use gpui_kit::component::input::{
    EditorState, InputEditsEvent, RangeDecoration, RangeDecorationCollection, RangeDecorationStyle,
    Redo, RopeExt as _, TabSize, Undo,
};
use gpui_kit::{
    App, AppContext as _, Entity, EntityInputHandler as _, Focusable as _, Hsla, Subscription,
    Window,
};
use ropey::extra::esoterica::ropes_are_instances;
use ropey::{LineType, Rope};
use std::collections::VecDeque;
use std::ops::Range;
use std::path::PathBuf;

use crate::language::{LanguageEntry, resolve_language};
use crate::shell::EditorViewPrefs;
/// Maximum replacement bytes materialized into `Vec<TextDelta>` on the UI thread per edit event.
/// Larger replacements (e.g. multi-hundred-KiB pastes or collapsed full-buffer IME commits) send
/// `deltas: Vec::new()` + the $O(1)$-cloned `committed_rope` so the worker writes a self-contained
/// checkpoint with zero large UI-thread `String` allocations.
pub const MAX_UI_DELTA_BYTES: usize = 64 * 1024;

/// Maximum ordinary editable document size accepted by `RecoveryService` (64 MiB).
pub const MAX_ORDINARY_DOC_BYTES: usize = 64 * 1024 * 1024;

/// Maximum bounding-span or output payload bytes materialized on the UI thread for
/// multi-edit batches with distinct replacement strings (`§11.1` / `§11.5`: 4 MiB).
/// Single edits and multi-edits with identical replacement text (`MultiIdentical`)
/// bypass bounding-span splicing altogether (copying only the replacement payload).
pub const MAX_BOUNDED_SPAN_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyEditsError {
    ReadOnly,
    CompositionActive,
    PendingNativeEdit,
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    OutOfBounds {
        range: Range<usize>,
        doc_len: usize,
    },
    InvalidRangeOrder {
        range: Range<usize>,
    },
    InvalidUtf8Boundary {
        offset: usize,
    },
    SplitsCrlf {
        offset: usize,
    },
    OverlappingOrDuplicateRanges {
        first: Range<usize>,
        second: Range<usize>,
    },
    ExceedsDocumentLimit {
        projected_bytes: usize,
        limit_bytes: usize,
    },
    LimitExceeded {
        span_bytes: usize,
        max_bytes: usize,
    },
}

impl std::fmt::Display for ApplyEditsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReadOnly => write!(f, "document is read-only"),
            Self::CompositionActive => {
                write!(
                    f,
                    "cannot apply programmatic edits while IME composition is active"
                )
            }
            Self::PendingNativeEdit => write!(
                f,
                "cannot apply programmatic edits while a prior native edit event is pending flush"
            ),
            Self::StaleRevision { expected, actual } => write!(
                f,
                "stale base revision: expected v{}, current document is v{}",
                expected.0, actual.0
            ),
            Self::OutOfBounds { range, doc_len } => write!(
                f,
                "edit range {}..{} exceeds document byte length {}",
                range.start, range.end, doc_len
            ),
            Self::InvalidRangeOrder { range } => write!(
                f,
                "edit range start {} exceeds end {}",
                range.start, range.end
            ),
            Self::InvalidUtf8Boundary { offset } => {
                write!(
                    f,
                    "byte offset {offset} is not on a UTF-8 character boundary"
                )
            }
            Self::SplitsCrlf { offset } => {
                write!(
                    f,
                    "byte offset {offset} splits a CRLF (\\r\\n) newline pair"
                )
            }
            Self::OverlappingOrDuplicateRanges { first, second } => write!(
                f,
                "overlapping or duplicate edit ranges {}..{} and {}..{} (caller must merge same-offset inserts)",
                first.start, first.end, second.start, second.end
            ),
            Self::ExceedsDocumentLimit {
                projected_bytes,
                limit_bytes,
            } => write!(
                f,
                "projected document size ({projected_bytes} B) exceeds ordinary editor limit ({limit_bytes} B)"
            ),
            Self::LimitExceeded {
                span_bytes,
                max_bytes,
            } => write!(
                f,
                "LIMIT_EXCEEDED: distinct multi-edit span ({span_bytes} B) exceeds {max_bytes} B UI atomic splice budget"
            ),
        }
    }
}

impl std::error::Error for ApplyEditsError {}

/// Pure validation & planning for atomic batch edits over a `Rope` snapshot (`§5.3`, `E02`, `E03`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValidatedEditPlan {
    NoOp,
    Single {
        range: Range<usize>,
        replacement: String,
    },
    MultiIdentical {
        ranges: Vec<Range<usize>>,
        replacement: String,
    },
    BoundedSpan {
        span: Range<usize>,
        spliced_replacement: String,
    },
}

pub fn validate_and_plan_edits(
    rope: &Rope,
    expected_base_rev: Revision,
    current_rev: Revision,
    is_editable: bool,
    ime_active: bool,
    edits: &[(Range<usize>, String)],
) -> Result<ValidatedEditPlan, ApplyEditsError> {
    if !is_editable {
        return Err(ApplyEditsError::ReadOnly);
    }
    if ime_active {
        return Err(ApplyEditsError::CompositionActive);
    }
    if expected_base_rev != current_rev {
        return Err(ApplyEditsError::StaleRevision {
            expected: expected_base_rev,
            actual: current_rev,
        });
    }
    if edits.is_empty() {
        return Ok(ValidatedEditPlan::NoOp);
    }

    let doc_len = rope.len();
    let mut indexed: Vec<(&Range<usize>, &str)> = Vec::with_capacity(edits.len());
    let mut net_delta: isize = 0;

    for (range, text) in edits {
        if range.start > range.end {
            return Err(ApplyEditsError::InvalidRangeOrder {
                range: range.clone(),
            });
        }
        if range.end > doc_len {
            return Err(ApplyEditsError::OutOfBounds {
                range: range.clone(),
                doc_len,
            });
        }
        if !rope.is_char_boundary(range.start) {
            return Err(ApplyEditsError::InvalidUtf8Boundary {
                offset: range.start,
            });
        }
        if !rope.is_char_boundary(range.end) {
            return Err(ApplyEditsError::InvalidUtf8Boundary { offset: range.end });
        }
        if splits_crlf(rope, range.start) {
            return Err(ApplyEditsError::SplitsCrlf {
                offset: range.start,
            });
        }
        if splits_crlf(rope, range.end) {
            return Err(ApplyEditsError::SplitsCrlf { offset: range.end });
        }

        net_delta += text.len() as isize - (range.end - range.start) as isize;
        indexed.push((range, text.as_str()));
    }

    let projected = doc_len.saturating_add_signed(net_delta);
    if projected > MAX_ORDINARY_DOC_BYTES {
        return Err(ApplyEditsError::ExceedsDocumentLimit {
            projected_bytes: projected,
            limit_bytes: MAX_ORDINARY_DOC_BYTES,
        });
    }

    indexed.sort_by(|a, b| {
        a.0.start
            .cmp(&b.0.start)
            .then_with(|| a.0.end.cmp(&b.0.end))
    });

    for pair in indexed.windows(2) {
        let prev = pair[0].0;
        let curr = pair[1].0;
        // Reject overlapping spans AND duplicate zero-length insertions at the exact same offset (§5.3).
        if prev.end > curr.start || (prev.start == curr.start && prev.end == curr.end) {
            return Err(ApplyEditsError::OverlappingOrDuplicateRanges {
                first: prev.clone(),
                second: curr.clone(),
            });
        }
    }

    // A1: Filter out identity edits AFTER full revision/bounds/UTF-8/CRLF/non-overlap validation
    // so no-op batches (`[(2..2, ""), (5..5, "")]`, `[(0..5, "hello")]`) never emit 0-undo
    // `InputEditsEvent`s or falsely advance `visible_revision` / dirty state.
    indexed.retain(|(range, text)| {
        !(range.len() == text.len() && rope_slice_eq_str(rope, (*range).clone(), text))
    });
    if indexed.is_empty() {
        return Ok(ValidatedEditPlan::NoOp);
    }

    if indexed.len() == 1 {
        let (range, text) = indexed[0];
        return Ok(ValidatedEditPlan::Single {
            range: range.clone(),
            replacement: text.to_string(),
        });
    }

    // Stock GPUI Kit 0.9.0 fast-path: strictly separated non-empty ranges with identical replacement text
    // execute natively as multi-selection `state.replace(...)` (1 Undo transaction, 1 InputEditsEvent).
    let first_text = indexed[0].1;
    let all_same_text = indexed.iter().all(|(_, t)| *t == first_text);
    let strictly_separated = indexed.windows(2).all(|w| w[0].0.end < w[1].0.start);
    if all_same_text && strictly_separated {
        return Ok(ValidatedEditPlan::MultiIdentical {
            ranges: indexed.into_iter().map(|(r, _)| r.clone()).collect(),
            replacement: first_text.to_string(),
        });
    }

    // General multi-edit with distinct replacement texts or adjacent ranges:
    // Splice untouched segments of `rope` across the minimal bounding span `first.start..last.end`.
    // Executes via a single `set_selected_range` + `state.replace` (`EditIntent::Atomic`,
    // `silent_replace_text = true`), guaranteeing 1 Undo entry, 1 `InputEditsEvent`, and 0 `set_value`.
    let span_start = indexed.first().expect("non-empty").0.start;
    let span_end = indexed.last().expect("non-empty").0.end;
    let source_span_bytes = span_end - span_start;
    if source_span_bytes > MAX_BOUNDED_SPAN_BYTES {
        return Err(ApplyEditsError::LimitExceeded {
            span_bytes: source_span_bytes,
            max_bytes: MAX_BOUNDED_SPAN_BYTES,
        });
    }
    let out_cap = source_span_bytes.saturating_add_signed(net_delta);
    if out_cap > MAX_BOUNDED_SPAN_BYTES {
        return Err(ApplyEditsError::LimitExceeded {
            span_bytes: out_cap,
            max_bytes: MAX_BOUNDED_SPAN_BYTES,
        });
    }
    let mut spliced = String::with_capacity(out_cap);
    let mut cursor = span_start;

    for (range, replacement) in indexed {
        if cursor < range.start {
            for chunk in rope.slice(cursor..range.start).chunks() {
                spliced.push_str(chunk);
            }
        }
        spliced.push_str(replacement);
        cursor = range.end;
    }
    if spliced.len() == source_span_bytes && rope_slice_eq_str(rope, span_start..span_end, &spliced)
    {
        return Ok(ValidatedEditPlan::NoOp);
    }

    Ok(ValidatedEditPlan::BoundedSpan {
        span: span_start..span_end,
        spliced_replacement: spliced,
    })
}

fn rope_slice_eq_str(rope: &Rope, range: Range<usize>, text: &str) -> bool {
    if range.len() != text.len() {
        return false;
    }
    let mut rem = text.as_bytes();
    for chunk in rope.slice(range).chunks() {
        let bytes = chunk.as_bytes();
        if !rem.starts_with(bytes) {
            return false;
        }
        rem = &rem[bytes.len()..];
    }
    rem.is_empty()
}

#[inline]
pub fn splits_crlf(rope: &Rope, offset: usize) -> bool {
    offset > 0
        && offset < rope.len()
        && rope.byte(offset - 1) == b'\r'
        && rope.byte(offset) == b'\n'
}

#[derive(Clone, Debug)]
pub struct CommittedMutationOutcome {
    pub new_rev: Revision,
    pub post_rope: Rope,
    pub savepoint_check: Option<PendingSavepointCheck>,
}

#[derive(Clone, Debug)]
struct ExpectedProgrammaticEvent {
    steps: Vec<(usize, usize, usize)>,
    post_rope: Rope,
}

#[cfg(test)]
impl ExpectedProgrammaticEvent {
    fn matches_batch(&self, steps: &[(usize, usize, usize)], final_rope: &Rope) -> bool {
        self.steps.as_slice() == steps && ropes_are_instances(final_rope, &self.post_rope)
    }
}

enum StagedMutationDeltas {
    ForceCheckpoint,
    Deltas(Vec<TextDelta>),
}

/// Exact contract predicate (`B3` Final Resolution):
/// - Fresh disk open (`saved_encoding == Some(encoding)` & `saved_line_ending == Some(line_ending)`) -> `false`
/// - Changed format on file-backed doc -> `true`
/// - Flipped back to captured baseline -> `false`
/// - Legacy recovered file-backed draft (`saved_encoding == None` or `saved_line_ending == None`) -> `true`
/// - Unsaved untitled (`saved_identity.is_none()`) -> `false`
#[inline]
pub fn compute_metadata_dirty(meta: &DocMeta) -> bool {
    meta.saved_identity.is_some()
        && (meta.saved_encoding != Some(meta.encoding)
            || meta.saved_line_ending != Some(meta.line_ending))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatChangeOutcome {
    NoOp,
    MetadataOnlyUntitled,
    RecoveryCommitRequired { new_rev: Revision },
}

/// Pure planner for B3 Exact Clause B format transitions.
#[inline]
pub fn classify_format_change(
    changed: bool,
    has_saved_identity: bool,
    committed_byte_len: u64,
    durable_rev: Revision,
    current_visible_rev: Revision,
) -> FormatChangeOutcome {
    if !changed {
        return FormatChangeOutcome::NoOp;
    }
    if has_saved_identity || committed_byte_len > 0 || durable_rev.0 > 0 {
        FormatChangeOutcome::RecoveryCommitRequired {
            new_rev: Revision(current_visible_rev.0.saturating_add(1)),
        }
    } else {
        FormatChangeOutcome::MetadataOnlyUntitled
    }
}

/// Pure $O(1)$ UI-thread savepoint dirty state tracker (`F03`, `F06`, zero UI hashing/O(n) scan).
#[derive(Clone, Debug)]
pub struct DirtyTracker {
    pub saved_identity: Option<ContentIdentity>,
    pub saved_revision: Option<Revision>,
    pub disk_conflict: bool,
    pub metadata_dirty: bool,
    is_dirty: bool,
    check_seq: u64,
    text_seq: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingSavepointCheck {
    pub seq: u64,
    pub checked_rev: Revision,
    pub checked_text_seq: u64,
    pub saved_identity: ContentIdentity,
}

impl DirtyTracker {
    pub fn new(
        saved_identity: Option<ContentIdentity>,
        saved_revision: Option<Revision>,
        disk_conflict: bool,
        initial_dirty: bool,
    ) -> Self {
        Self {
            saved_identity,
            saved_revision,
            disk_conflict,
            metadata_dirty: false,
            is_dirty: initial_dirty,
            check_seq: 0,
            text_seq: 0,
        }
    }

    #[inline]
    pub fn with_metadata_dirty(mut self, metadata_dirty: bool) -> Self {
        self.metadata_dirty = metadata_dirty;
        self
    }

    #[inline]
    pub fn text_seq(&self) -> u64 {
        self.text_seq
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.is_dirty || self.metadata_dirty || self.disk_conflict
    }

    /// True if buffer content or encoding/EOL metadata differs from the last saved identity
    /// (independent of external `disk_conflict`).
    #[inline]
    pub fn has_local_edits(&self) -> bool {
        self.is_dirty || self.metadata_dirty
    }

    /// Advances revision tracking for a pure metadata format change (`0` Rope text mutation).
    /// If Rope content was clean at `old_rev`, advances `saved_revision` to `new_rev` so flipping
    /// format back to saved baseline clears dirty immediately in $O(1)$.
    #[inline]
    pub fn on_metadata_revision_advance(
        &mut self,
        old_rev: Revision,
        new_rev: Revision,
        metadata_dirty: bool,
    ) {
        self.metadata_dirty = metadata_dirty;
        if self.saved_revision == Some(old_rev) {
            self.saved_revision = Some(new_rev);
        }
    }

    /// Evaluates dirty state in strictly $O(1)$ time on the UI thread after a committed edit.
    /// Returns `Some(PendingSavepointCheck)` ONLY when byte length equals `saved_identity.byte_len`
    /// at a different revision (e.g. Undo back to savepoint `F06` or equal-length edit), so a
    /// background worker can run `ContentIdentity::matches_rope` off the UI thread.
    pub fn on_committed_edit(
        &mut self,
        new_rev: Revision,
        new_byte_len: u64,
    ) -> Option<PendingSavepointCheck> {
        self.text_seq = self.text_seq.wrapping_add(1);
        self.check_seq = self.check_seq.wrapping_add(1);

        if self.disk_conflict {
            self.is_dirty = true;
            return None;
        }

        let Some(saved_id) = self.saved_identity else {
            self.is_dirty = new_byte_len > 0;
            return None;
        };

        if self.saved_revision == Some(new_rev) {
            self.is_dirty = false;
            return None;
        }

        if new_byte_len != saved_id.byte_len {
            self.is_dirty = true;
            return None;
        }

        // Equal byte length, different revision: optimistically mark dirty on UI thread (never
        // risks losing edits) and request coalesced worker hash verification.
        self.is_dirty = true;
        Some(PendingSavepointCheck {
            seq: self.check_seq,
            checked_rev: new_rev,
            checked_text_seq: self.text_seq,
            saved_identity: saved_id,
        })
    }

    /// Applies the background worker's `ContentIdentity::matches_rope` result if still current.
    pub fn apply_worker_check(
        &mut self,
        seq: u64,
        checked_rev: Revision,
        current_visible_rev: Revision,
        matches_saved: bool,
    ) -> bool {
        if self.check_seq != seq || checked_rev != current_visible_rev || self.disk_conflict {
            return false;
        }
        if matches_saved {
            self.is_dirty = false;
            self.saved_revision = Some(checked_rev);
        } else {
            self.is_dirty = true;
        }
        true
    }

    /// Applies background savepoint verification keyed on `text_seq` so concurrent format-only
    /// revision advances do not invalidate an exact Rope match.
    pub fn apply_worker_check_for_text_seq(
        &mut self,
        seq: u64,
        checked_text_seq: u64,
        current_visible_rev: Revision,
        matches_saved: bool,
    ) -> bool {
        if self.check_seq != seq || checked_text_seq != self.text_seq || self.disk_conflict {
            return false;
        }
        if matches_saved {
            self.is_dirty = false;
            self.saved_revision = Some(current_visible_rev);
        } else {
            self.is_dirty = true;
        }
        true
    }

    /// Updates the savepoint when an atomic disk save completes for `captured_rev` (`F03`, `F06`).
    pub fn on_saved(
        &mut self,
        captured_rev: Revision,
        captured_identity: ContentIdentity,
        current_visible_rev: Revision,
        current_byte_len: u64,
    ) -> Option<PendingSavepointCheck> {
        let text_seq_for_capture = if current_visible_rev == captured_rev {
            self.text_seq
        } else {
            self.text_seq.wrapping_sub(1)
        };
        self.on_saved_with_format(
            captured_rev,
            text_seq_for_capture,
            captured_identity,
            current_visible_rev,
            current_byte_len,
            false,
        )
    }

    /// Updates the savepoint using CAPTURED save formats (`captured_text_seq` distinguishes
    /// pure format-change-during-save from text-edit-during-save in $O(1)$).
    pub fn on_saved_with_format(
        &mut self,
        captured_rev: Revision,
        captured_text_seq: u64,
        captured_identity: ContentIdentity,
        current_visible_rev: Revision,
        current_byte_len: u64,
        metadata_dirty: bool,
    ) -> Option<PendingSavepointCheck> {
        self.saved_identity = Some(captured_identity);
        self.disk_conflict = false;
        self.metadata_dirty = metadata_dirty;
        self.check_seq = self.check_seq.wrapping_add(1);

        if current_visible_rev == captured_rev || self.text_seq == captured_text_seq {
            self.saved_revision = Some(current_visible_rev);
            self.is_dirty = false;
            return None;
        }

        self.saved_revision = Some(captured_rev);
        if current_byte_len != captured_identity.byte_len {
            self.is_dirty = true;
            return None;
        }

        self.is_dirty = true;
        Some(PendingSavepointCheck {
            seq: self.check_seq,
            checked_rev: current_visible_rev,
            checked_text_seq: self.text_seq,
            saved_identity: captured_identity,
        })
    }
}

pub struct HydrateEditorParams {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub meta: DocMeta,
    pub rope: Rope,
    pub initial_rev: Revision,
    pub durable_rev: Revision,
    pub is_clean_at_load: bool,
    pub disk_conflict: bool,
    pub initial_view: ViewState,
    pub initial_prefs: EditorViewPrefs,
    pub activation_tick: u64,
}

/// Adapter around GPUI Kit's `Entity<EditorState>` — the sole mutable text and undo authority
/// for an ordinary editable document (`B1`, `B2`, `B3`, `B6`).
pub struct EditorAdapter {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub meta: DocMeta,
    pub language: LanguageEntry,

    editor: Entity<EditorState>,
    search_decorations: RangeDecorationCollection,

    /// Last committed `Rope` from load or `InputEditsEvent` (`O(1)` `Arc` clone; excludes IME preedit).
    committed_rope: Rope,

    pub visible_revision: Revision,
    pub queued_revision: Revision,
    pub durable_revision: Revision,
    pub(crate) has_checkpoint: bool,

    dirty: DirtyTracker,
    has_undo_history: bool,
    pub last_activated_tick: u64,
    applied_prefs: EditorViewPrefs,
    pending_base_rev: Option<Revision>,
    pending_deltas: Vec<TextDelta>,
    pending_force_checkpoint: bool,

    pending_programmatic_acks: VecDeque<ExpectedProgrammaticEvent>,
    pub oversized_recovery_warned: bool,
    suppress_events: bool,
    subscription: Option<[Subscription; 2]>,
}

impl EditorAdapter {
    pub fn new(params: HydrateEditorParams, window: &mut Window, cx: &mut App) -> Self {
        let language = resolve_language(
            params.meta.path.as_deref(),
            Some(&params.rope),
            params.meta.language_override.as_deref(),
        );
        let is_readonly = params.meta.readonly_reason.is_some()
            || params.meta.encoding == EncodingSpec::UnsupportedReadOnly
            || params.meta.line_ending == LineEnding::MixedReadOnly;
        let initial_prefs = params.initial_prefs;

        let initial_cursor = if params.initial_view.cursor_byte > 0 {
            let clamped = (params.initial_view.cursor_byte as usize).min(params.rope.len());
            let safe = params.rope.floor_char_boundary(clamped);
            Some(if splits_crlf(&params.rope, safe) {
                safe.saturating_sub(1)
            } else {
                safe
            })
        } else {
            None
        };

        let editor = cx.new(|cx| {
            // CRITICAL (`search.rs:283` guard): `EditorState::new` defaults `searchable = true`.
            // Explicitly set `.searchable(false)` so `Cmd+F` / `Cmd+Shift+F` propagate to our
            // workspace search bar and never hit upstream's UTF-8-vs-UTF-16 `replace_all_search_matches`.
            // Note: `.language(...)` initializes `LayoutMode::CodeEditor`, so `.tab_size(...)` MUST follow it.
            let mut state = EditorState::new_with_rope(params.rope, window, cx)
                .searchable(false)
                .language(language.id)
                .soft_wrap(initial_prefs.soft_wrap)
                .show_whitespaces(initial_prefs.show_whitespaces)
                .tab_size(TabSize {
                    tab_size: initial_prefs.tab_size as usize,
                    hard_tabs: initial_prefs.hard_tabs,
                });
            if is_readonly {
                state.set_readonly(true, cx);
            }
            if let Some(safe) = initial_cursor {
                state.set_selected_range(safe..safe, cx);
            }
            state
        });

        // Clone the canonical native text after construction so the committed snapshot
        // shares its exact underlying Rope identity, including before the first layout.
        let committed_rope = editor.read(cx).text().clone();

        let search_decorations = editor.update(cx, |state, cx| {
            state.create_range_decorations_collection(Vec::new(), cx)
        });

        // Load callers compare the worker-computed identity; never hash the full buffer here.
        let initial_content_dirty = !params.is_clean_at_load;

        let saved_revision = if !initial_content_dirty && !params.disk_conflict {
            Some(params.initial_rev)
        } else {
            None
        };

        let initial_metadata_dirty = compute_metadata_dirty(&params.meta);
        let dirty = DirtyTracker::new(
            params.meta.saved_identity,
            saved_revision,
            params.disk_conflict,
            initial_content_dirty,
        )
        .with_metadata_dirty(initial_metadata_dirty);
        let force_checkpoint = params.durable_rev.0 == 0 && dirty.is_dirty();

        Self {
            doc_id: params.doc_id,
            lifecycle_gen: params.lifecycle_gen,
            meta: params.meta,
            language,
            editor,
            search_decorations,
            committed_rope,
            visible_revision: params.initial_rev,
            queued_revision: params.durable_rev,
            durable_revision: params.durable_rev,
            has_checkpoint: false,
            dirty,
            has_undo_history: false,
            last_activated_tick: params.activation_tick,
            applied_prefs: initial_prefs,
            pending_base_rev: None,
            pending_deltas: Vec::new(),
            pending_force_checkpoint: force_checkpoint,
            pending_programmatic_acks: VecDeque::new(),
            oversized_recovery_warned: false,
            suppress_events: false,
            subscription: None,
        }
    }

    pub fn attach_subscription(&mut self, subscription: [Subscription; 2]) {
        self.subscription = Some(subscription);
    }

    #[inline]
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    #[inline]
    pub fn committed_rope(&self) -> &Rope {
        &self.committed_rope
    }

    #[inline]
    pub fn committed_byte_len(&self) -> u64 {
        self.committed_rope.len() as u64
    }

    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty.is_dirty()
    }

    #[inline]
    pub fn has_undo_history(&self) -> bool {
        self.has_undo_history
    }

    #[inline]
    pub fn has_disk_conflict(&self) -> bool {
        self.dirty.disk_conflict
    }

    #[inline]
    pub fn has_local_edits(&self) -> bool {
        self.dirty.has_local_edits()
    }

    #[inline]
    pub fn current_text_seq(&self) -> u64 {
        self.dirty.text_seq()
    }

    pub(crate) fn has_pending_native_edit(&self, cx: &App) -> bool {
        !ropes_are_instances(self.editor.read(cx).text(), &self.committed_rope)
    }

    #[inline]
    pub fn applied_prefs(&self) -> EditorViewPrefs {
        self.applied_prefs
    }

    /// Applies global editor view preferences if they differ from `self.applied_prefs`
    /// (zero dirty/revision/undo mutation).
    pub fn apply_view_prefs(
        &mut self,
        prefs: EditorViewPrefs,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.applied_prefs == prefs {
            return false;
        }
        let prev = self.applied_prefs;
        self.applied_prefs = prefs;
        self.editor.update(cx, |state, cx| {
            if prev.soft_wrap != prefs.soft_wrap {
                state.set_soft_wrap(prefs.soft_wrap, window, cx);
            }
            if prev.show_whitespaces != prefs.show_whitespaces {
                state.set_show_whitespaces(prefs.show_whitespaces, window, cx);
            }
            if prev.tab_size != prefs.tab_size || prev.hard_tabs != prefs.hard_tabs {
                state.set_tab_size(
                    TabSize {
                        tab_size: prefs.tab_size as usize,
                        hard_tabs: prefs.hard_tabs,
                    },
                    cx,
                );
            }
        });
        true
    }
    #[inline]
    pub fn is_readonly(&self) -> bool {
        self.meta.readonly_reason.is_some()
            || self.meta.encoding == EncodingSpec::UnsupportedReadOnly
            || self.meta.line_ending == LineEnding::MixedReadOnly
    }

    pub fn has_active_ime(&self, window: &mut Window, cx: &mut App) -> bool {
        self.editor.update(cx, |state, cx| {
            state.marked_text_range(window, cx).is_some()
        })
    }

    fn record_committed_mutation(
        &mut self,
        post_rope: Rope,
        staged: StagedMutationDeltas,
    ) -> CommittedMutationOutcome {
        let base_rev = self.visible_revision;
        let new_rev = Revision(base_rev.0.saturating_add(1));
        self.visible_revision = new_rev;
        self.committed_rope = post_rope.clone();
        self.has_undo_history = true;

        if self.pending_base_rev.is_none() {
            self.pending_base_rev = Some(base_rev);
        }
        match staged {
            StagedMutationDeltas::ForceCheckpoint => {
                self.pending_force_checkpoint = true;
                self.pending_deltas.clear();
            }
            StagedMutationDeltas::Deltas(new_deltas) => {
                if self.pending_force_checkpoint {
                    self.pending_deltas.clear();
                } else {
                    self.pending_deltas.extend(new_deltas);
                    let retained_delta_bytes: usize = self
                        .pending_deltas
                        .iter()
                        .map(|d| d.replacement.len() + 64)
                        .sum();
                    if retained_delta_bytes > MAX_UI_DELTA_BYTES * 8
                        || self.pending_deltas.len() > 2048
                    {
                        self.pending_force_checkpoint = true;
                        self.pending_deltas.clear();
                    }
                }
            }
        }

        let savepoint_check = self
            .dirty
            .on_committed_edit(new_rev, post_rope.len() as u64);
        CommittedMutationOutcome {
            new_rev,
            post_rope,
            savepoint_check,
        }
    }

    /// Processes a single committed `InputEditsEvent` batch (`B2`: never fires during IME preedit).
    /// Consumes matching `ExpectedProgrammaticEvent` acknowledgements in $O(1)$ without
    /// double-incrementing `visible_revision`; ingests genuine native edits via `record_committed_mutation`.
    pub fn ingest_edits_event(
        &mut self,
        event: &InputEditsEvent,
    ) -> Option<CommittedMutationOutcome> {
        if self.suppress_events {
            return None;
        }
        let edits = event.edits();
        let Some((_, final_rope)) = edits.last() else {
            return None;
        };

        if let Some(expected) = self.pending_programmatic_acks.front() {
            let shape_matches = edits.len() == expected.steps.len()
                && edits
                    .iter()
                    .zip(expected.steps.iter())
                    .all(|((e, _), &(s, oe, ne))| {
                        e.start_byte == s && e.old_end_byte == oe && e.new_end_byte == ne
                    })
                && ropes_are_instances(final_rope, &expected.post_rope);
            if shape_matches {
                self.pending_programmatic_acks.pop_front();
                return None;
            }
        }

        let total_inserted: usize = edits
            .iter()
            .map(|(edit, _)| edit.new_end_byte.saturating_sub(edit.start_byte))
            .sum();

        let staged = if self.pending_force_checkpoint || total_inserted > MAX_UI_DELTA_BYTES {
            StagedMutationDeltas::ForceCheckpoint
        } else {
            let mut deltas = Vec::with_capacity(edits.len());
            for (edit, post_rope) in edits {
                let start = edit.start_byte.min(post_rope.len());
                let new_end = edit.new_end_byte.min(post_rope.len()).max(start);
                let replacement = post_rope.slice(start..new_end).to_string();
                deltas.push(TextDelta {
                    byte_range: (edit.start_byte as u64)..(edit.old_end_byte as u64),
                    replacement,
                });
            }
            StagedMutationDeltas::Deltas(deltas)
        };

        Some(self.record_committed_mutation(final_rope.clone(), staged))
    }

    pub fn apply_savepoint_verification(&mut self, seq: u64, checked_text_seq: u64, matches: bool) {
        self.dirty.apply_worker_check_for_text_seq(
            seq,
            checked_text_seq,
            self.visible_revision,
            matches,
        );
    }

    /// Builds a `RecoveryCommit` for all unqueued edits if `visible_revision > queued_revision`
    /// and the document fits within the 64 MiB ordinary-document recovery cap.
    pub fn build_recovery_commit(&self) -> Option<RecoveryCommit> {
        if self.visible_revision <= self.queued_revision {
            return None;
        }
        if self.committed_rope.len() > MAX_ORDINARY_DOC_BYTES {
            return None;
        }
        let base_rev = self.pending_base_rev.unwrap_or(self.queued_revision);
        let deltas = if self.pending_force_checkpoint || self.durable_revision.0 == 0 {
            Vec::new()
        } else {
            self.pending_deltas.clone()
        };
        Some(RecoveryCommit {
            doc_id: self.doc_id,
            lifecycle_gen: self.lifecycle_gen,
            base_rev,
            new_rev: self.visible_revision,
            rope: self.committed_rope.clone(),
            deltas,
            meta: self.meta.clone(),
        })
    }

    pub fn record_commit_queued(&mut self, new_rev: Revision) {
        if new_rev >= self.queued_revision {
            self.queued_revision = new_rev;
            if self.queued_revision >= self.visible_revision {
                self.pending_base_rev = None;
                self.pending_deltas.clear();
                self.pending_force_checkpoint = false;
            }
        }
    }

    /// When `RecoveryHandle::try_commit` returns `Busy`, drop incremental deltas and retain only
    /// the latest `committed_rope` (`O(1)`) so the next retry sends a self-contained checkpoint.
    pub fn record_commit_busy(&mut self) {
        self.pending_force_checkpoint = true;
        self.pending_deltas.clear();
        if self.pending_base_rev.is_none() {
            self.pending_base_rev = Some(self.queued_revision);
        }
    }

    pub fn mark_durable_ack(&mut self, lifecycle_gen: LifecycleGen, durable_rev: Revision) {
        if lifecycle_gen == self.lifecycle_gen {
            self.has_checkpoint = true;
        }
        if lifecycle_gen == self.lifecycle_gen && durable_rev > self.durable_revision {
            self.durable_revision = durable_rev;
            if self.queued_revision < durable_rev {
                self.queued_revision = durable_rev;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn mark_saved(
        &mut self,
        target_path: PathBuf,
        captured_rev: Revision,
        captured_text_seq: u64,
        captured_identity: ContentIdentity,
        fingerprint: FileFingerprint,
        captured_encoding: EncodingSpec,
        captured_line_ending: LineEnding,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<PendingSavepointCheck> {
        let new_title = target_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("untitled")
            .to_string();
        let path_changed = self.meta.path.as_ref() != Some(&target_path);
        self.meta.title = new_title;
        self.meta.path = Some(target_path);
        self.meta.origin_fingerprint = Some(fingerprint);
        self.meta.saved_identity = Some(captured_identity);
        // B3 Exact Clause A: assign saved baseline from CAPTURED save request formats, never current choices!
        self.meta.saved_encoding = Some(captured_encoding);
        self.meta.saved_line_ending = Some(captured_line_ending);

        if path_changed && self.meta.language_override.is_none() {
            let detected = resolve_language(
                self.meta.path.as_deref(),
                Some(&self.committed_rope),
                self.meta.language_override.as_deref(),
            );
            if detected != self.language {
                self.set_language_entry(detected, window, cx);
            }
        }

        let metadata_dirty = compute_metadata_dirty(&self.meta);
        self.dirty.on_saved_with_format(
            captured_rev,
            captured_text_seq,
            captured_identity,
            self.visible_revision,
            self.committed_rope.len() as u64,
            metadata_dirty,
        )
    }

    /// Mandatory atomic programmatic batch edits (`§5.3`, `E02`, `E03`).
    /// Validates `expected_base_rev`, editability, active IME, UTF-8 char boundaries, `\r\n` pairs,
    /// and non-overlapping/non-duplicate ranges before mutating `EditorState`.
    /// Executes as one Undo transaction and advances `visible_revision` once.
    pub(crate) fn apply_edits(
        &mut self,
        expected_base_rev: Revision,
        edits: &[(Range<usize>, String)],
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(Revision, Option<CommittedMutationOutcome>), ApplyEditsError> {
        let is_editable = !self.is_readonly() && self.editor.read(cx).is_editable();
        if !is_editable {
            return Err(ApplyEditsError::ReadOnly);
        }
        let ime_active = self.has_active_ime(window, cx);
        if ime_active {
            return Err(ApplyEditsError::CompositionActive);
        }
        if !ropes_are_instances(self.editor.read(cx).text(), &self.committed_rope) {
            return Err(ApplyEditsError::PendingNativeEdit);
        }

        let plan = validate_and_plan_edits(
            &self.committed_rope,
            expected_base_rev,
            self.visible_revision,
            true,
            false,
            edits,
        )?;

        match plan {
            ValidatedEditPlan::NoOp => Ok((self.visible_revision, None)),
            ValidatedEditPlan::Single { range, replacement } => {
                let step = (range.start, range.end, range.start + replacement.len());
                let staged = if replacement.len() > MAX_UI_DELTA_BYTES {
                    StagedMutationDeltas::ForceCheckpoint
                } else {
                    StagedMutationDeltas::Deltas(vec![TextDelta {
                        byte_range: (range.start as u64)..(range.end as u64),
                        replacement: replacement.clone(),
                    }])
                };
                let post_rope = self.editor.update(cx, |state, cx| {
                    let saved_scroll = state.scroll_offset();
                    state.set_selected_range(range, cx);
                    state.replace(replacement, window, cx);
                    state.set_scroll_offset(saved_scroll, cx);
                    state.text().clone()
                });
                self.pending_programmatic_acks
                    .push_back(ExpectedProgrammaticEvent {
                        steps: vec![step],
                        post_rope: post_rope.clone(),
                    });
                let outcome = self.record_committed_mutation(post_rope, staged);
                Ok((outcome.new_rev, Some(outcome)))
            }
            ValidatedEditPlan::MultiIdentical {
                ranges,
                replacement,
            } => {
                let steps: Vec<(usize, usize, usize)> = ranges
                    .iter()
                    .rev()
                    .map(|r| (r.start, r.end, r.start + replacement.len()))
                    .collect();
                let total_inserted = replacement.len().saturating_mul(ranges.len());
                let staged = if total_inserted > MAX_UI_DELTA_BYTES {
                    StagedMutationDeltas::ForceCheckpoint
                } else {
                    StagedMutationDeltas::Deltas(
                        ranges
                            .iter()
                            .rev()
                            .map(|r| TextDelta {
                                byte_range: (r.start as u64)..(r.end as u64),
                                replacement: replacement.clone(),
                            })
                            .collect(),
                    )
                };
                let post_rope = self.editor.update(cx, |state, cx| {
                    let saved_scroll = state.scroll_offset();
                    if let Some((first, rest)) = ranges.split_first() {
                        state.set_selected_range(first.clone(), cx);
                        for range in rest {
                            state.add_selection(range.clone(), cx);
                        }
                        state.replace(replacement, window, cx);
                    }
                    state.set_scroll_offset(saved_scroll, cx);
                    state.text().clone()
                });
                self.pending_programmatic_acks
                    .push_back(ExpectedProgrammaticEvent {
                        steps,
                        post_rope: post_rope.clone(),
                    });
                let outcome = self.record_committed_mutation(post_rope, staged);
                Ok((outcome.new_rev, Some(outcome)))
            }
            ValidatedEditPlan::BoundedSpan {
                span,
                spliced_replacement,
            } => {
                let step = (span.start, span.end, span.start + spliced_replacement.len());
                let staged = if spliced_replacement.len() > MAX_UI_DELTA_BYTES {
                    StagedMutationDeltas::ForceCheckpoint
                } else {
                    StagedMutationDeltas::Deltas(vec![TextDelta {
                        byte_range: (span.start as u64)..(span.end as u64),
                        replacement: spliced_replacement.clone(),
                    }])
                };
                let post_rope = self.editor.update(cx, |state, cx| {
                    let saved_scroll = state.scroll_offset();
                    state.set_selected_range(span, cx);
                    state.replace(spliced_replacement, window, cx);
                    state.set_scroll_offset(saved_scroll, cx);
                    state.text().clone()
                });
                self.pending_programmatic_acks
                    .push_back(ExpectedProgrammaticEvent {
                        steps: vec![step],
                        post_rope: post_rope.clone(),
                    });
                let outcome = self.record_committed_mutation(post_rope, staged);
                Ok((outcome.new_rev, Some(outcome)))
            }
        }
    }

    pub fn undo(&self, window: &mut Window, cx: &mut App) {
        let focus = self.editor.read(cx).focus_handle(cx);
        focus.focus(window, cx);
        window.dispatch_action(Box::new(Undo), cx);
    }

    pub fn redo(&self, window: &mut Window, cx: &mut App) {
        let focus = self.editor.read(cx).focus_handle(cx);
        focus.focus(window, cx);
        window.dispatch_action(Box::new(Redo), cx);
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.editor.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Returns `(cursor_byte, 1-based line, 1-based char col)` from `committed_rope` in $O(\log N)$.
    pub fn cursor_metrics(&self, cx: &App) -> (usize, usize, usize) {
        let state = self.editor.read(cx);
        let raw_cursor = state.cursor().min(self.committed_rope.len());
        let cursor = self.committed_rope.floor_char_boundary(raw_cursor);
        let line_idx = self.committed_rope.byte_to_line_idx(cursor, LineType::LF);
        let line_start = self.committed_rope.line_to_byte_idx(line_idx, LineType::LF);
        let col_chars = self
            .committed_rope
            .byte_to_char_idx(cursor)
            .saturating_sub(self.committed_rope.byte_to_char_idx(line_start));
        (cursor, line_idx + 1, col_chars + 1)
    }

    pub fn capture_view_state(&self, cx: &App) -> ViewState {
        let state = self.editor.read(cx);
        let cursor = state.cursor().min(self.committed_rope.len()) as u64;
        ViewState {
            cursor_byte: cursor,
            scroll_byte: 0,
        }
    }

    pub fn jump_to_byte_range(&self, range: Range<usize>, window: &mut Window, cx: &mut App) {
        let len = self.committed_rope.len();
        let mut start = self
            .committed_rope
            .floor_char_boundary(range.start.min(len));
        let mut end = self.committed_rope.ceil_char_boundary(range.end.min(len));
        if splits_crlf(&self.committed_rope, start) {
            start = start.saturating_sub(1);
        }
        if splits_crlf(&self.committed_rope, end) {
            end = (end + 1).min(len);
        }
        self.editor.update(cx, |state, cx| {
            let pos = state.text().offset_to_position(start);
            state.unfold_at(pos, cx);
            state.set_selected_range(start..end, cx);
            state.focus(window, cx);
        });
    }

    pub fn jump_to_line_1based(&self, line_1based: usize, window: &mut Window, cx: &mut App) {
        let total_lines = self.committed_rope.len_lines(LineType::LF).max(1);
        let target_line = line_1based.saturating_sub(1).min(total_lines - 1);
        let byte_offset = self
            .committed_rope
            .line_to_byte_idx(target_line, LineType::LF);
        self.jump_to_byte_range(byte_offset..byte_offset, window, cx);
    }

    pub fn set_search_highlights(
        &self,
        ranges: &[Range<usize>],
        active_ix: Option<usize>,
        fill_color: Hsla,
        active_color: Hsla,
        cx: &mut App,
    ) {
        let cap = ranges.len().min(1024);
        let mut decorations = Vec::with_capacity(cap + 1);
        for (ix, range) in ranges.iter().take(cap).enumerate() {
            if Some(ix) == active_ix {
                continue;
            }
            decorations.push(
                RangeDecoration::new(range.clone())
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(fill_color),
            );
        }
        if let Some(ix) = active_ix
            && let Some(active_range) = ranges.get(ix)
        {
            decorations.push(
                RangeDecoration::new(active_range.clone())
                    .with_style(RangeDecorationStyle::Fill)
                    .with_color(active_color),
            );
            decorations.push(
                RangeDecoration::new(active_range.clone())
                    .with_style(RangeDecorationStyle::Frame)
                    .with_color(active_color),
            );
        }
        self.search_decorations.set(decorations, cx);
    }

    pub fn clear_search_highlights(&self, cx: &mut App) {
        self.search_decorations.clear(cx);
    }

    /// Explicit user-invoked line ending switch (`LF` <-> `CRLF`). Never allowed on read-only docs.
    pub fn set_line_ending(
        &mut self,
        line_ending: LineEnding,
    ) -> Result<FormatChangeOutcome, &'static str> {
        if self.is_readonly() {
            return Err(
                "Document is read-only; Save As or reopen after resolving encoding/mixed EOL",
            );
        }
        if line_ending == LineEnding::MixedReadOnly {
            return Err("Cannot switch target line ending to MixedReadOnly");
        }
        let changed = self.meta.line_ending != line_ending;
        let outcome = classify_format_change(
            changed,
            self.meta.saved_identity.is_some(),
            self.committed_byte_len(),
            self.durable_revision,
            self.visible_revision,
        );
        match outcome {
            FormatChangeOutcome::NoOp => {}
            FormatChangeOutcome::MetadataOnlyUntitled => {
                self.meta.line_ending = line_ending;
                self.dirty.metadata_dirty = false;
            }
            FormatChangeOutcome::RecoveryCommitRequired { new_rev } => {
                let old_rev = self.visible_revision;
                self.meta.line_ending = line_ending;
                self.visible_revision = new_rev;
                if self.pending_base_rev.is_none() {
                    self.pending_base_rev = Some(old_rev);
                }
                self.pending_force_checkpoint = true;
                self.pending_deltas.clear();
                self.dirty.on_metadata_revision_advance(
                    old_rev,
                    new_rev,
                    compute_metadata_dirty(&self.meta),
                );
            }
        }
        Ok(outcome)
    }

    /// Explicit user-invoked encoding switch (`Utf8` <-> `Utf8Bom`). Never allowed on read-only docs.
    pub fn set_encoding(
        &mut self,
        encoding: EncodingSpec,
    ) -> Result<FormatChangeOutcome, &'static str> {
        if self.is_readonly() {
            return Err("Document is read-only; unsupported encoding cannot switch BOM");
        }
        if encoding == EncodingSpec::UnsupportedReadOnly {
            return Err("Cannot switch target encoding to UnsupportedReadOnly");
        }
        let changed = self.meta.encoding != encoding;
        let outcome = classify_format_change(
            changed,
            self.meta.saved_identity.is_some(),
            self.committed_byte_len(),
            self.durable_revision,
            self.visible_revision,
        );
        match outcome {
            FormatChangeOutcome::NoOp => {}
            FormatChangeOutcome::MetadataOnlyUntitled => {
                self.meta.encoding = encoding;
                self.dirty.metadata_dirty = false;
            }
            FormatChangeOutcome::RecoveryCommitRequired { new_rev } => {
                let old_rev = self.visible_revision;
                self.meta.encoding = encoding;
                self.visible_revision = new_rev;
                if self.pending_base_rev.is_none() {
                    self.pending_base_rev = Some(old_rev);
                }
                self.pending_force_checkpoint = true;
                self.pending_deltas.clear();
                self.dirty.on_metadata_revision_advance(
                    old_rev,
                    new_rev,
                    compute_metadata_dirty(&self.meta),
                );
            }
        }
        Ok(outcome)
    }

    /// Explicit user-invoked UTF-8 BOM toggle (`Utf8` <-> `Utf8Bom`). Never allowed on read-only docs.
    pub fn toggle_utf8_bom(&mut self) -> Result<(EncodingSpec, FormatChangeOutcome), &'static str> {
        if self.is_readonly() {
            return Err("Document is read-only; unsupported encoding cannot toggle BOM");
        }
        let next = match self.meta.encoding {
            EncodingSpec::Utf8 => EncodingSpec::Utf8Bom,
            EncodingSpec::Utf8Bom => EncodingSpec::Utf8,
            EncodingSpec::UnsupportedReadOnly => {
                return Err("Unsupported encoding is read-only");
            }
        };
        let outcome = self.set_encoding(next)?;
        Ok((next, outcome))
    }

    pub fn set_language_entry(&mut self, entry: LanguageEntry, _window: &mut Window, cx: &mut App) {
        self.language = entry;
        self.editor.update(cx, |state, cx| {
            state.set_highlighter(entry.id, cx);
        });
    }

    /// Updates per-document `DocMeta.language_override` (`None` = Auto Detect) without dirtying
    /// or advancing `visible_revision`.
    pub fn set_language_override(
        &mut self,
        override_id: Option<&str>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let normalized = override_id.map(|s| s.to_string());
        let next_entry = resolve_language(
            self.meta.path.as_deref(),
            Some(&self.committed_rope),
            normalized.as_deref(),
        );
        let meta_changed = self.meta.language_override != normalized;
        self.meta.language_override = normalized;
        if self.language != next_entry {
            self.set_language_entry(next_entry, window, cx);
            true
        } else {
            meta_changed
        }
    }
}

#[cfg(test)]
#[path = "editor_adapter_ime_tests.rs"]
mod ime_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_stale_revision_utf8_mid_char_and_crlf_split() {
        let rope = Rope::from_str("中😀\r\nworld");
        // "中" = 0..3, "😀" = 3..7, "\r\n" = 7..9, "world" = 9..14

        // Stale revision (E03)
        let stale = validate_and_plan_edits(
            &rope,
            Revision(2),
            Revision(3),
            true,
            false,
            &[(0..3, "A".into())],
        );
        assert_eq!(
            stale,
            Err(ApplyEditsError::StaleRevision {
                expected: Revision(2),
                actual: Revision(3),
            })
        );

        // Inside UTF-8 3-byte char '中'
        let bad_utf8 = validate_and_plan_edits(
            &rope,
            Revision(3),
            Revision(3),
            true,
            false,
            &[(1..3, "A".into())],
        );
        assert_eq!(
            bad_utf8,
            Err(ApplyEditsError::InvalidUtf8Boundary { offset: 1 })
        );

        // Inside UTF-8 4-byte emoji '😀'
        let bad_emoji = validate_and_plan_edits(
            &rope,
            Revision(3),
            Revision(3),
            true,
            false,
            &[(3..5, "A".into())],
        );
        assert_eq!(
            bad_emoji,
            Err(ApplyEditsError::InvalidUtf8Boundary { offset: 5 })
        );

        // Splitting CRLF between '\r' (7) and '\n' (8)
        let bad_crlf = validate_and_plan_edits(
            &rope,
            Revision(3),
            Revision(3),
            true,
            false,
            &[(0..8, "A".into())],
        );
        assert_eq!(bad_crlf, Err(ApplyEditsError::SplitsCrlf { offset: 8 }));
    }

    #[test]
    fn rejects_overlapping_and_duplicate_same_offset_insertions() {
        let rope = Rope::from_str("hello world");

        let overlap = validate_and_plan_edits(
            &rope,
            Revision(1),
            Revision(1),
            true,
            false,
            &[(0..6, "A".into()), (5..11, "B".into())],
        );
        assert!(matches!(
            overlap,
            Err(ApplyEditsError::OverlappingOrDuplicateRanges { .. })
        ));

        let same_point_inserts = validate_and_plan_edits(
            &rope,
            Revision(1),
            Revision(1),
            true,
            false,
            &[(5..5, "A".into()), (5..5, "B".into())],
        );
        assert!(matches!(
            same_point_inserts,
            Err(ApplyEditsError::OverlappingOrDuplicateRanges { .. })
        ));
    }

    #[test]
    fn plans_multi_identical_and_distinct_spliced_span_correctly() {
        let rope = Rope::from_str("hello world");

        // Identical replacements on disjoint ranges -> MultiIdentical
        let same_plan = validate_and_plan_edits(
            &rope,
            Revision(5),
            Revision(5),
            true,
            false,
            &[(6..11, "X".into()), (0..5, "X".into())],
        )
        .expect("valid multi identical");
        assert_eq!(
            same_plan,
            ValidatedEditPlan::MultiIdentical {
                ranges: vec![0..5, 6..11],
                replacement: "X".into(),
            }
        );

        // Distinct replacements (§11.4 example: "hello"->"你好", "world"->"世界") -> BoundedSpan
        let distinct_plan = validate_and_plan_edits(
            &rope,
            Revision(107),
            Revision(107),
            true,
            false,
            &[(0..5, "你好".into()), (6..11, "世界".into())],
        )
        .expect("valid distinct batch");
        assert_eq!(
            distinct_plan,
            ValidatedEditPlan::BoundedSpan {
                span: 0..11,
                spliced_replacement: "你好 世界".into(),
            }
        );
    }

    #[test]
    fn dirty_tracker_handles_save_during_typing_and_undo_to_savepoint_without_ui_hashing() {
        let initial_rope = Rope::from_str("alpha");
        let saved_id = ContentIdentity::of_rope(&initial_rope);
        let mut tracker = DirtyTracker::new(Some(saved_id), Some(Revision(0)), false, false);
        assert!(!tracker.is_dirty());

        // Type '!' -> len 6 != 5 -> O(1) dirty without worker check
        assert_eq!(tracker.on_committed_edit(Revision(1), 6), None);
        assert!(tracker.is_dirty());

        // Undo back to "alpha" -> Revision(2), len 5 == saved_id.byte_len -> requests worker check
        let check = tracker
            .on_committed_edit(Revision(2), 5)
            .expect("equal-length edit/undo requests background verification");
        assert_eq!(check.checked_rev, Revision(2));
        assert!(tracker.is_dirty()); // Optimistically dirty until worker confirms

        // Worker confirms `saved_id.matches_rope(&committed_rope) == true` (F06)
        assert!(tracker.apply_worker_check(check.seq, Revision(2), Revision(2), true));
        assert!(!tracker.is_dirty());

        // Save captured Revision(3) ("alpha+"), while user already typed Revision(4) ("alpha++") (F03)
        let saved_v3_rope = Rope::from_str("alpha+");
        let saved_v3_id = ContentIdentity::of_rope(&saved_v3_rope);
        let _ = tracker.on_committed_edit(Revision(3), 6);
        let _ = tracker.on_committed_edit(Revision(4), 7);
        assert_eq!(
            tracker.on_saved(Revision(3), saved_v3_id, Revision(4), 7),
            None
        );
        assert!(
            tracker.is_dirty(),
            "subsequent edit after captured save stays dirty"
        );
    }

    #[test]
    fn budgets_distinct_bounding_span_at_4mib_while_allowing_wide_identical_replacements() {
        let mut text = "a".repeat(MAX_BOUNDED_SPAN_BYTES + 1024);
        text.push_str("tail");
        let rope = Rope::from_str(&text);
        let end = rope.len();

        // Identical replacement text at byte 0..1 and near EOF (> 4 MiB apart) uses MultiIdentical
        // with zero disjoint span splicing!
        let identical = vec![(0..1, "Z".to_string()), ((end - 4)..end, "Z".to_string())];
        let plan =
            validate_and_plan_edits(&rope, Revision(1), Revision(1), true, false, &identical)
                .expect("identical replacements across >4MiB succeed without span splice");
        assert!(matches!(plan, ValidatedEditPlan::MultiIdentical { .. }));

        // Distinct replacement texts across > 4 MiB bounding span reject with LimitExceeded
        // before allocating or mutating the buffer.
        let distinct = vec![(0..1, "X".to_string()), ((end - 4)..end, "Y".to_string())];
        let err = validate_and_plan_edits(&rope, Revision(1), Revision(1), true, false, &distinct)
            .expect_err("distinct replacements spanning >4MiB must return LimitExceeded");
        assert!(matches!(
            err,
            ApplyEditsError::LimitExceeded {
                max_bytes: MAX_BOUNDED_SPAN_BYTES,
                ..
            }
        ));
    }

    #[test]
    fn identity_and_cancelling_edits_plan_as_noop_while_preserving_boundary_validation() {
        let rope = Rope::from_str("中hello\r\nworld");
        // "中" = 0..3, "hello" = 3..8, "\r\n" = 8..10, "world" = 10..15

        // 1. Multi-empty insertions at distinct valid boundaries -> NoOp
        let multi_empty = validate_and_plan_edits(
            &rope,
            Revision(4),
            Revision(4),
            true,
            false,
            &[(3..3, String::new()), (8..8, String::new())],
        )
        .expect("valid multi-empty");
        assert_eq!(multi_empty, ValidatedEditPlan::NoOp);

        // 2. Same-text replacement -> NoOp
        let same_text = validate_and_plan_edits(
            &rope,
            Revision(4),
            Revision(4),
            true,
            false,
            &[(3..8, "hello".to_string()), (10..15, "world".to_string())],
        )
        .expect("valid same-text");
        assert_eq!(same_text, ValidatedEditPlan::NoOp);

        // 3. Adjacent cancelling edits (`[(0..0, "a"), (0..1, "")]` on `"a"`) -> NoOp
        let rope_a = Rope::from_str("a");
        let cancelling = validate_and_plan_edits(
            &rope_a,
            Revision(1),
            Revision(1),
            true,
            false,
            &[(0..0, "a".to_string()), (0..1, String::new())],
        )
        .expect("valid cancelling adjacent edits");
        assert_eq!(cancelling, ValidatedEditPlan::NoOp);

        // 4. Invalid UTF-8 boundary or CRLF split in otherwise-identity edits MUST still fail!
        let bad_utf8_empty = validate_and_plan_edits(
            &rope,
            Revision(4),
            Revision(4),
            true,
            false,
            &[(1..1, String::new())],
        );
        assert_eq!(
            bad_utf8_empty,
            Err(ApplyEditsError::InvalidUtf8Boundary { offset: 1 })
        );

        let bad_crlf_identity = validate_and_plan_edits(
            &rope,
            Revision(4),
            Revision(4),
            true,
            false,
            &[(3..9, "hello\r".to_string())],
        );
        assert_eq!(
            bad_crlf_identity,
            Err(ApplyEditsError::SplitsCrlf { offset: 9 })
        );
    }

    #[test]
    fn expected_programmatic_event_matches_immutable_clone_across_chained_cow_mutations() {
        let r0 = Rope::from_str("hello world");
        let mut state_rope = r0.clone();
        assert!(ropes_are_instances(&state_rope, &r0));

        // Programmatic edit #1: "hello" -> "greetings"
        state_rope.remove(0..5);
        state_rope.insert(0, "greetings");
        let event1_rope = state_rope.clone();
        let exp1 = ExpectedProgrammaticEvent {
            steps: vec![(0, 5, 9)],
            post_rope: state_rope.clone(),
        };

        // Chained programmatic edit #2 in the same update COW-mutates `state_rope`:
        state_rope.remove(10..15);
        state_rope.insert(10, "GPUI!");
        let event2_rope = state_rope.clone();
        let exp2 = ExpectedProgrammaticEvent {
            steps: vec![(10, 15, 15)],
            post_rope: state_rope.clone(),
        };

        // Event #1 still matches `exp1` via immutable clone instance even after `state_rope` mutated!
        assert!(exp1.matches_batch(&[(0, 5, 9)], &event1_rope));
        assert!(!exp1.matches_batch(&[(0, 5, 9)], &event2_rope));
        assert!(exp2.matches_batch(&[(10, 15, 15)], &event2_rope));

        // Equal-length native edit with identical shape `(10, 15, 15)` produces a different Rope instance -> mismatch!
        let mut native_equal_len = event1_rope.clone();
        native_equal_len.remove(10..15);
        native_equal_len.insert(10, "EARTH");
        assert!(!exp2.matches_batch(&[(10, 15, 15)], &native_equal_len));
    }

    #[test]
    fn format_dirty_predicate_flip_back_legacy_baseline_and_save_during_format_change() {
        let rope = Rope::from_str("hello\n");
        let saved_id = ContentIdentity::of_rope(&rope);

        // 1. Fresh disk open -> clean
        let mut meta = DocMeta {
            title: "note.txt".into(),
            path: Some(PathBuf::from("/tmp/note.txt")),
            encoding: EncodingSpec::Utf8,
            line_ending: LineEnding::Lf,
            saved_identity: Some(saved_id),
            saved_encoding: Some(EncodingSpec::Utf8),
            saved_line_ending: Some(LineEnding::Lf),
            ..DocMeta::default()
        };
        assert!(!compute_metadata_dirty(&meta));

        // 2. Changed format -> dirty; flip back -> clean
        meta.encoding = EncodingSpec::Utf8Bom;
        assert!(compute_metadata_dirty(&meta));
        meta.encoding = EncodingSpec::Utf8;
        assert!(!compute_metadata_dirty(&meta));

        // 3. Legacy recovered file-backed draft (`saved_encoding == None`) -> conservatively dirty!
        let legacy_meta = DocMeta {
            saved_identity: Some(saved_id),
            encoding: EncodingSpec::Utf8,
            line_ending: LineEnding::Lf,
            saved_encoding: None,
            saved_line_ending: None,
            ..DocMeta::default()
        };
        assert!(compute_metadata_dirty(&legacy_meta));

        // 4. Unsaved untitled (`saved_identity == None`) -> never metadata_dirty
        let untitled_meta = DocMeta {
            saved_identity: None,
            encoding: EncodingSpec::Utf8Bom,
            line_ending: LineEnding::Crlf,
            ..DocMeta::default()
        };
        assert!(!compute_metadata_dirty(&untitled_meta));

        // 5. Save-in-flight while user changes format (`Utf8 -> Utf8Bom`), then flips back!
        let mut tracker = DirtyTracker::new(Some(saved_id), Some(Revision(0)), false, false);
        let edited_rope = Rope::from_str("hello world\n");
        let edited_id = ContentIdentity::of_rope(&edited_rope);
        let _ = tracker.on_committed_edit(Revision(1), edited_rope.len() as u64);
        let captured_rev = Revision(1);
        let captured_text_seq = tracker.text_seq();

        // While save of Revision(1) (Utf8, Lf) is in flight, user switches to Utf8Bom -> Revision(2):
        meta.encoding = EncodingSpec::Utf8Bom;
        tracker.on_metadata_revision_advance(
            Revision(1),
            Revision(2),
            compute_metadata_dirty(&meta),
        );

        // Save of Revision(1) finishes with CAPTURED (Utf8, Lf) -> still dirty because current is Utf8Bom!
        meta.saved_identity = Some(edited_id);
        meta.saved_encoding = Some(EncodingSpec::Utf8);
        meta.saved_line_ending = Some(LineEnding::Lf);
        assert_eq!(
            tracker.on_saved_with_format(
                captured_rev,
                captured_text_seq,
                edited_id,
                Revision(2),
                edited_rope.len() as u64,
                compute_metadata_dirty(&meta),
            ),
            None
        );
        assert!(
            tracker.is_dirty(),
            "format changed during in-flight save must remain dirty"
        );

        // User flips Utf8Bom back to captured Utf8 -> Revision(3) -> clean immediately in O(1)!
        meta.encoding = EncodingSpec::Utf8;
        tracker.on_metadata_revision_advance(
            Revision(2),
            Revision(3),
            compute_metadata_dirty(&meta),
        );
        assert!(
            !tracker.is_dirty(),
            "flip-back to captured format must clear dirty"
        );
    }

    #[test]
    fn format_change_classification_separates_0byte_untitled_from_tracked_buffers() {
        // Identity re-selection -> strict NoOp
        assert_eq!(
            classify_format_change(false, true, 128, Revision(2), Revision(2)),
            FormatChangeOutcome::NoOp
        );
        // 0-byte untouched untitled -> MetadataOnlyUntitled (revision stays 0, no recovery_heads row)
        assert_eq!(
            classify_format_change(true, false, 0, Revision(0), Revision(0)),
            FormatChangeOutcome::MetadataOnlyUntitled
        );
        // Non-empty untitled -> advances revision & forces checkpoint
        assert_eq!(
            classify_format_change(true, false, 5, Revision(0), Revision(1)),
            FormatChangeOutcome::RecoveryCommitRequired {
                new_rev: Revision(2)
            }
        );
        // File-backed (even 0 bytes) -> advances revision & forces checkpoint
        assert_eq!(
            classify_format_change(true, true, 0, Revision(0), Revision(0)),
            FormatChangeOutcome::RecoveryCommitRequired {
                new_rev: Revision(1)
            }
        );
    }
}
