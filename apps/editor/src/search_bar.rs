use std::ops::Range;

use editor_services::{
    CancelToken, DocumentId, ErrorKind, QueryId, Revision, SearchAlgorithm, SearchEvent,
    SearchMatch, SearchRequest, SearchService, SearchTarget, SearchTask, TryRecvError,
};
use regex::RegexBuilder;
use ropey::{LineType, Rope};

use crate::editor_adapter::splits_crlf;

/// Maximum matches retained for display (`§15.2`: 10,000 cap).
pub const MAX_SEARCH_RESULTS: usize = 10_000;

/// Maximum document byte size allowed for full-snapshot Regex / Case-Insensitive scan on Editable tabs (16 MiB).
pub const MAX_REGEX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchQueryKind {
    /// Exact UTF-8 literal streaming search via `editor-services` (`MemchrBaseline` or `ByteScanExperiment`).
    LiteralStreaming,
    /// Case-insensitive literal search (supported on Editable documents).
    CaseInsensitive,
    /// Bounded Rust `regex` search & capture-group replace (supported on Editable documents; explicitly rejected on Paged large files per `S06`).
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Idle,
    Running {
        emitted: usize,
    },
    Done {
        total_emitted: usize,
        truncated: bool,
        count_exact: bool,
        elapsed_ns: u64,
        bytes_scanned: u64,
    },
    Cancelled {
        emitted_before_cancel: usize,
    },
    Invalidated,
    Error(String),
}

pub struct SearchSessionState {
    pub visible: bool,
    pub replace_visible: bool,
    pub query_kind: SearchQueryKind,
    pub algorithm: SearchAlgorithm,

    pub next_query_id: u64,
    pub active_query_id: Option<QueryId>,
    pub target_doc_id: Option<DocumentId>,
    pub target_editable_rev: Option<Revision>,
    pub last_query_text: String,

    pub matches: Vec<SearchMatch>,
    pub selected_index: Option<usize>,
    pub status: SearchStatus,

    active_task: Option<SearchTask>,
    pending_retry_request: Option<SearchRequest>,
    extended_cancel: Option<CancelToken>,
}

impl Default for SearchSessionState {
    fn default() -> Self {
        Self {
            visible: false,
            replace_visible: false,
            query_kind: SearchQueryKind::LiteralStreaming,
            algorithm: SearchAlgorithm::MemchrBaseline,
            next_query_id: 1,
            active_query_id: None,
            target_doc_id: None,
            target_editable_rev: None,
            last_query_text: String::new(),
            matches: Vec::new(),
            selected_index: None,
            status: SearchStatus::Idle,
            active_task: None,
            pending_retry_request: None,
            extended_cancel: None,
        }
    }
}

impl SearchSessionState {
    pub fn cancel_active(&mut self) {
        let had_pending = self.pending_retry_request.take().is_some();
        let had_extended = if let Some(cancel) = self.extended_cancel.take() {
            cancel.cancel();
            true
        } else {
            false
        };
        let had_task = if let Some(task) = self.active_task.take() {
            task.cancel.cancel();
            true
        } else {
            false
        };
        if (had_pending || had_extended || had_task)
            && matches!(self.status, SearchStatus::Running { .. })
        {
            self.status = SearchStatus::Cancelled {
                emitted_before_cancel: self.matches.len(),
            };
        }
        self.active_query_id = None;
    }

    /// Cancels any active snapshot search and drops stale match byte ranges when `doc_id`
    /// advances to `new_rev` (`U-B3` / `S05`).
    pub fn invalidate_for_doc_edit(&mut self, doc_id: DocumentId, new_rev: Revision) -> bool {
        if self.target_doc_id != Some(doc_id) || self.target_editable_rev == Some(new_rev) {
            return false;
        }
        let was_active_or_populated = self.is_running()
            || !self.matches.is_empty()
            || matches!(self.status, SearchStatus::Done { .. });
        self.cancel_active();
        self.matches.clear();
        self.selected_index = None;
        self.target_editable_rev = None;
        if was_active_or_populated && !self.last_query_text.is_empty() {
            self.status = SearchStatus::Invalidated;
        }
        was_active_or_populated
    }

    /// Prepares an asynchronous background Regex / Case-Insensitive scan (`U-B3`).
    pub fn begin_background_extended_search(
        &mut self,
        doc_id: DocumentId,
        rev: Revision,
        pattern: &str,
    ) -> (QueryId, CancelToken) {
        self.cancel_active();
        self.matches.clear();
        self.selected_index = None;
        self.target_doc_id = Some(doc_id);
        self.target_editable_rev = Some(rev);
        self.last_query_text = pattern.to_string();

        let qid = self.allocate_query_id();
        let cancel = CancelToken::new();
        self.active_query_id = Some(qid);
        self.extended_cancel = Some(cancel.clone());
        self.status = SearchStatus::Running { emitted: 0 };
        (qid, cancel)
    }

    /// Applies the background extended-search result iff `(qid, current_doc_rev)` still matches.
    pub fn apply_extended_search_result(
        &mut self,
        qid: QueryId,
        current_doc_rev: Option<Revision>,
        result: Result<Option<EditableSearchOutput>, String>,
    ) -> bool {
        if self.active_query_id != Some(qid) {
            return false;
        }
        self.active_query_id = None;
        self.extended_cancel = None;

        if self.target_editable_rev != current_doc_rev {
            self.matches.clear();
            self.selected_index = None;
            self.status = SearchStatus::Invalidated;
            return true;
        }

        match result {
            Ok(Some(out)) => {
                let count = out.matches.len();
                self.matches = out.matches;
                self.selected_index = (!self.matches.is_empty()).then_some(0);
                self.status = SearchStatus::Done {
                    total_emitted: count,
                    truncated: out.truncated,
                    count_exact: !out.truncated,
                    elapsed_ns: out.elapsed_ns,
                    bytes_scanned: out.bytes_scanned,
                };
            }
            Ok(None) => {
                self.status = SearchStatus::Cancelled {
                    emitted_before_cancel: 0,
                };
            }
            Err(msg) => {
                self.matches.clear();
                self.selected_index = None;
                self.status = SearchStatus::Error(msg);
            }
        }
        true
    }

    pub fn clear_results(&mut self) {
        self.cancel_active();
        self.matches.clear();
        self.selected_index = None;
        self.target_editable_rev = None;
        self.status = SearchStatus::Idle;
    }

    pub fn cycle_algorithm(&mut self) -> SearchAlgorithm {
        self.algorithm = match self.algorithm {
            SearchAlgorithm::MemchrBaseline => SearchAlgorithm::ByteScanExperiment,
            SearchAlgorithm::ByteScanExperiment => SearchAlgorithm::MemchrBaseline,
        };
        self.algorithm
    }

    pub fn allocate_query_id(&mut self) -> QueryId {
        let id = QueryId(self.next_query_id);
        self.next_query_id = self.next_query_id.wrapping_add(1);
        id
    }

    /// Starts a streaming UTF-8 literal search via `editor_services::SearchService` (`§7.2`, `S01`–`S05`).
    pub fn start_streaming_literal(
        &mut self,
        service: &SearchService,
        pattern: &str,
        target: SearchTarget,
    ) {
        self.cancel_active();
        self.matches.clear();
        self.selected_index = None;
        self.last_query_text = pattern.to_string();

        let (doc_id, rev) = match &target {
            SearchTarget::EditableSnapshot { doc_id, rev, .. } => (*doc_id, Some(*rev)),
            SearchTarget::PagedFile { handle, .. } => (handle.doc_id(), None),
        };
        self.target_doc_id = Some(doc_id);
        self.target_editable_rev = rev;

        if pattern.is_empty() {
            self.status = SearchStatus::Idle;
            return;
        }

        let query_id = self.allocate_query_id();
        let mut req = SearchRequest::new(query_id, pattern.as_bytes().to_vec(), target);
        req.algorithm = self.algorithm;
        req.max_matches = MAX_SEARCH_RESULTS;

        match service.start(req.clone()) {
            Ok(task) => {
                self.active_query_id = Some(query_id);
                self.active_task = Some(task);
                self.status = SearchStatus::Running { emitted: 0 };
            }
            Err(err) if err.kind == ErrorKind::Busy => {
                // Distinct-doc worker pool saturated: retain latest queued request and retry async.
                self.active_query_id = Some(query_id);
                self.pending_retry_request = Some(req);
                self.status = SearchStatus::Running { emitted: 0 };
            }
            Err(err) => {
                self.status = SearchStatus::Error(err.message);
            }
        }
    }

    /// Retries a queued `ErrorKind::Busy` search request without blocking the UI thread.
    pub fn try_flush_pending_start(&mut self, service: &SearchService) -> bool {
        let Some(req) = self.pending_retry_request.clone() else {
            return false;
        };
        match service.start(req) {
            Ok(task) => {
                self.pending_retry_request = None;
                self.active_task = Some(task);
                true
            }
            Err(err) if err.kind == ErrorKind::Busy => false,
            Err(err) => {
                self.pending_retry_request = None;
                self.active_query_id = None;
                self.status = SearchStatus::Error(err.message);
                true
            }
        }
    }
    /// Drains up to `max_packets` available events from `SearchTask` without blocking UI.
    /// Returns `true` if state changed and UI should repaint.
    pub fn poll_streaming_events(&mut self, max_packets: usize) -> bool {
        let Some(expected_qid) = self.active_query_id else {
            return false;
        };
        let Some(task) = self.active_task.as_ref() else {
            return false;
        };

        let mut changed = false;
        let mut finished = false;

        for _ in 0..max_packets {
            match task.events.try_recv() {
                Ok(event) => match event {
                    SearchEvent::Batch { query_id, matches } => {
                        if query_id != expected_qid {
                            continue;
                        }
                        let remaining = MAX_SEARCH_RESULTS.saturating_sub(self.matches.len());
                        self.matches.extend(matches.into_iter().take(remaining));
                        if self.selected_index.is_none() && !self.matches.is_empty() {
                            self.selected_index = Some(0);
                        }
                        self.status = SearchStatus::Running {
                            emitted: self.matches.len(),
                        };
                        changed = true;
                    }
                    SearchEvent::Done {
                        query_id,
                        total_emitted,
                        truncated,
                        count_exact,
                        elapsed_ns,
                        bytes_scanned,
                        ..
                    } => {
                        if query_id != expected_qid {
                            continue;
                        }
                        if self.selected_index.is_none() && !self.matches.is_empty() {
                            self.selected_index = Some(0);
                        }
                        self.status = SearchStatus::Done {
                            total_emitted,
                            truncated,
                            count_exact,
                            elapsed_ns,
                            bytes_scanned,
                        };
                        finished = true;
                        changed = true;
                        break;
                    }
                    SearchEvent::Cancelled { query_id } => {
                        if query_id != expected_qid {
                            continue;
                        }
                        self.status = SearchStatus::Cancelled {
                            emitted_before_cancel: self.matches.len(),
                        };
                        finished = true;
                        changed = true;
                        break;
                    }
                    SearchEvent::Invalidated { query_id, .. } => {
                        if query_id != expected_qid {
                            continue;
                        }
                        self.status = SearchStatus::Invalidated;
                        finished = true;
                        changed = true;
                        break;
                    }
                    SearchEvent::Failed { query_id, error } => {
                        if query_id != expected_qid {
                            continue;
                        }
                        self.status = SearchStatus::Error(error.message);
                        finished = true;
                        changed = true;
                        break;
                    }
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if matches!(self.status, SearchStatus::Running { .. }) {
                        self.status = SearchStatus::Cancelled {
                            emitted_before_cancel: self.matches.len(),
                        };
                        changed = true;
                    }
                    finished = true;
                    break;
                }
            }
        }

        if finished {
            self.active_task = None;
            self.active_query_id = None;
        }
        changed
    }

    pub fn is_running(&self) -> bool {
        self.active_task.is_some()
            || self.pending_retry_request.is_some()
            || self.extended_cancel.is_some()
    }

    pub fn select_next(&mut self) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            self.selected_index = None;
            return None;
        }
        let next = match self.selected_index {
            Some(ix) if ix + 1 < self.matches.len() => ix + 1,
            _ => 0,
        };
        self.selected_index = Some(next);
        self.matches.get(next)
    }

    pub fn select_prev(&mut self) -> Option<&SearchMatch> {
        if self.matches.is_empty() {
            self.selected_index = None;
            return None;
        }
        let prev = match self.selected_index {
            Some(ix) if ix > 0 => ix - 1,
            _ => self.matches.len() - 1,
        };
        self.selected_index = Some(prev);
        self.matches.get(prev)
    }

    pub fn status_summary(&self) -> String {
        match &self.status {
            SearchStatus::Idle => "Ready".to_string(),
            SearchStatus::Running { emitted } => format!("Scanning… {emitted} matches found"),
            SearchStatus::Done {
                total_emitted,
                truncated,
                count_exact,
                elapsed_ns,
                ..
            } => {
                let ms = (*elapsed_ns as f64) / 1_000_000.0;
                if *truncated || !*count_exact {
                    format!(
                        "{total_emitted}+ matches (capped at {MAX_SEARCH_RESULTS}; exact total not scanned · {ms:.1} ms)"
                    )
                } else {
                    let pos = self
                        .selected_index
                        .map(|i| format!("{}/", i + 1))
                        .unwrap_or_default();
                    format!("{pos}{total_emitted} matches ({ms:.1} ms)")
                }
            }
            SearchStatus::Cancelled {
                emitted_before_cancel,
            } => format!("Cancelled ({emitted_before_cancel} partial matches)"),
            SearchStatus::Invalidated => {
                "Invalidated — underlying file changed during scan".to_string()
            }
            SearchStatus::Error(msg) => format!("Error: {msg}"),
        }
    }
}

/// Bounded snapshot search + optional replacement planner for Editable documents
/// (`CaseInsensitive` and `Regex` modes, plus capture-aware `replace_all` edits).
pub struct EditableSearchOutput {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
    pub elapsed_ns: u64,
    pub bytes_scanned: u64,
}

pub fn run_editable_extended_search(
    rope: &Rope,
    pattern: &str,
    kind: SearchQueryKind,
    cancel: &CancelToken,
) -> Result<Option<EditableSearchOutput>, String> {
    if cancel.is_cancelled() {
        return Ok(None);
    }
    if pattern.is_empty() {
        return Ok(Some(EditableSearchOutput {
            matches: Vec::new(),
            truncated: false,
            elapsed_ns: 0,
            bytes_scanned: 0,
        }));
    }
    if pattern.len() > 65_536 {
        return Err("Query exceeds 64 KiB limit".to_string());
    }
    if rope.len() > MAX_REGEX_SNAPSHOT_BYTES {
        return Err(format!(
            "Document ({:.1} MiB) exceeds 16 MiB Regex/Case-Fold snapshot budget; use Literal search",
            rope.len() as f64 / (1024.0 * 1024.0)
        ));
    }

    let started = std::time::Instant::now();
    let regex_pattern = match kind {
        SearchQueryKind::CaseInsensitive => format!("(?i){}", regex::escape(pattern)),
        SearchQueryKind::Regex => pattern.to_string(),
        SearchQueryKind::LiteralStreaming => regex::escape(pattern),
    };

    let re = RegexBuilder::new(&regex_pattern)
        .size_limit(4 * 1024 * 1024)
        .dfa_size_limit(4 * 1024 * 1024)
        .build()
        .map_err(|e| format!("Invalid regular expression: {e}"))?;

    let mut text = String::with_capacity(rope.len());
    for (chunk_ix, chunk) in rope.chunks().enumerate() {
        if chunk_ix % 64 == 0 && cancel.is_cancelled() {
            return Ok(None);
        }
        text.push_str(chunk);
    }

    let mut matches = Vec::new();
    let mut truncated = false;
    let mut cursor = 0usize;
    let mut iter_count = 0usize;

    while cursor <= text.len() {
        iter_count = iter_count.wrapping_add(1);
        if iter_count % 64 == 0 && cancel.is_cancelled() {
            return Ok(None);
        }
        let Some(m) = re.find_at(&text, cursor) else {
            break;
        };
        let range = m.start()..m.end();
        // Never emit a match that splits a CRLF pair (§4.4).
        if !splits_crlf(rope, range.start) && !splits_crlf(rope, range.end) {
            if matches.len() >= MAX_SEARCH_RESULTS {
                truncated = true;
                break;
            }
            // Store 0-based line_number (matching `editor_services::SearchMatch` convention).
            let line_0 = rope.byte_to_line_idx(range.start, LineType::LF) as u64;
            let (preview, preview_match_range) = bounded_preview(&text, range.clone());
            matches.push(SearchMatch {
                byte_range: (range.start as u64)..(range.end as u64),
                line_number: Some(line_0),
                preview,
                preview_match_range,
            });
        }

        // Advance cleanly on zero-length regex matches along UTF-8 char boundaries (§7.2).
        if range.start == range.end {
            if range.end >= text.len() {
                break;
            }
            cursor = text[range.end..]
                .chars()
                .next()
                .map(|ch| range.end + ch.len_utf8())
                .unwrap_or(text.len() + 1);
            if splits_crlf(rope, cursor) {
                cursor += 1;
            }
        } else {
            cursor = range.end;
        }
    }

    Ok(Some(EditableSearchOutput {
        matches,
        truncated,
        elapsed_ns: started.elapsed().as_nanos() as u64,
        bytes_scanned: text.len() as u64,
    }))
}

/// Builds atomic `(Range<usize>, String)` edits for Replace Current or Replace All on an Editable
/// `Rope` snapshot (`§7.4`), supporting `$1`/`$0` regex capture expansion in `Regex` mode.
pub fn plan_replacement_edits(
    rope: &Rope,
    pattern: &str,
    replacement: &str,
    kind: SearchQueryKind,
    matches: &[SearchMatch],
    single_index: Option<usize>,
) -> Result<Vec<(Range<usize>, String)>, String> {
    if matches.is_empty() {
        return Ok(Vec::new());
    }

    let selected: &[SearchMatch] = match single_index {
        Some(ix) => {
            let Some(m) = matches.get(ix) else {
                return Ok(Vec::new());
            };
            std::slice::from_ref(m)
        }
        None => matches,
    };

    if kind != SearchQueryKind::Regex {
        let mut edits = Vec::with_capacity(selected.len());
        for m in selected {
            let start = usize::try_from(m.byte_range.start)
                .map_err(|_| "Match offset exceeds addressable memory")?;
            let end = usize::try_from(m.byte_range.end)
                .map_err(|_| "Match offset exceeds addressable memory")?;
            edits.push((start..end, replacement.to_string()));
        }
        return Ok(edits);
    }

    let re = RegexBuilder::new(pattern)
        .size_limit(4 * 1024 * 1024)
        .build()
        .map_err(|e| format!("Invalid regular expression: {e}"))?;

    let mut edits = Vec::with_capacity(selected.len());
    for m in selected {
        let start = usize::try_from(m.byte_range.start)
            .map_err(|_| "Match offset exceeds addressable memory")?;
        let end = usize::try_from(m.byte_range.end)
            .map_err(|_| "Match offset exceeds addressable memory")?;
        if end > rope.len() || !rope.is_char_boundary(start) || !rope.is_char_boundary(end) {
            return Err("Search match no longer aligns with current document snapshot".to_string());
        }
        let matched_str = rope.slice(start..end).to_string();
        let expanded = if let Some(caps) = re.captures(&matched_str) {
            let mut dst = String::new();
            caps.expand(replacement, &mut dst);
            dst
        } else {
            replacement.to_string()
        };
        edits.push((start..end, expanded));
    }
    Ok(edits)
}

fn bounded_preview(text: &str, range: Range<usize>) -> (Box<str>, Range<usize>) {
    let mut lo = range.start.saturating_sub(64);
    while lo < range.start && !text.is_char_boundary(lo) {
        lo += 1;
    }
    let mut hi = (lo + 256).min(text.len());
    while hi > lo && !text.is_char_boundary(hi) {
        hi -= 1;
    }
    let slice = &text[lo..hi];
    let match_start = range.start.saturating_sub(lo).min(slice.len());
    let match_end = range.end.saturating_sub(lo).min(slice.len());
    (slice.into(), match_start..match_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_zero_length_matches_and_0based_line_numbers_are_exact() {
        let rope = Rope::from_str("中\r\n文");
        let cancel = CancelToken::new();
        let out = run_editable_extended_search(&rope, "^", SearchQueryKind::Regex, &cancel)
            .expect("valid regex")
            .expect("not cancelled");
        assert_eq!(out.matches.len(), 1);
        assert_eq!(out.matches[0].byte_range, 0..0);
        assert_eq!(out.matches[0].line_number, Some(0)); // 0-based -> UI displays Ln 1

        let out_line2 = run_editable_extended_search(&rope, "文", SearchQueryKind::Regex, &cancel)
            .expect("valid regex")
            .expect("not cancelled");
        assert_eq!(out_line2.matches.len(), 1);
        assert_eq!(out_line2.matches[0].line_number, Some(1)); // 0-based -> UI displays Ln 2
    }

    #[test]
    fn extended_search_honors_cancellation_and_edit_invalidation() {
        let rope = Rope::from_str("alpha\nbeta\ngamma\n");
        let pre_cancelled = CancelToken::new();
        pre_cancelled.cancel();
        let cancelled_res = run_editable_extended_search(
            &rope,
            "beta",
            SearchQueryKind::CaseInsensitive,
            &pre_cancelled,
        )
        .expect("valid query");
        assert!(cancelled_res.is_none());

        let doc_id = DocumentId::new();
        let mut session = SearchSessionState::default();
        let (qid, token) = session.begin_background_extended_search(doc_id, Revision(3), "beta");
        assert!(session.is_running());
        assert!(!token.is_cancelled());

        // Editing document to Revision(4) cancels running extended search and marks Invalidated.
        assert!(session.invalidate_for_doc_edit(doc_id, Revision(4)));
        assert!(token.is_cancelled());
        assert!(!session.is_running());
        assert_eq!(session.status, SearchStatus::Invalidated);

        // Late worker completion for qid is ignored cleanly.
        assert!(!session.apply_extended_search_result(qid, Some(Revision(4)), Ok(None)));
    }

    #[test]
    fn regex_capture_replacement_expands_groups_cleanly() {
        let rope = Rope::from_str("item_10, item_20");
        let cancel = CancelToken::new();
        let out =
            run_editable_extended_search(&rope, r"item_(\d+)", SearchQueryKind::Regex, &cancel)
                .expect("regex search")
                .expect("not cancelled");
        assert_eq!(out.matches.len(), 2);

        let edits = plan_replacement_edits(
            &rope,
            r"item_(\d+)",
            "[$1]",
            SearchQueryKind::Regex,
            &out.matches,
            None,
        )
        .expect("plan regex replace");
        assert_eq!(
            edits,
            vec![(0..7, "[10]".to_string()), (9..16, "[20]".to_string())]
        );
    }
}
