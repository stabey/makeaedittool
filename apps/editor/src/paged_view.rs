use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use editor_services::{
    CancelToken, DocMeta, DocumentId, EncodingSpec, IndexStatus, LifecycleGen, LineEnding,
    PagedDocHandle, ReadOnlyReason, SourceGeneration, VIEWPORT_BYTES, ViewState, ViewportAnchor,
    ViewportRequest, ViewportSlice,
};
use gpui_kit::component::VirtualListScrollHandle;
use gpui_kit::{Pixels, ScrollStrategy, Size, px, size};

/// Step size for PageUp / PageDown navigation inside the 64 KiB bounded viewport (48 KiB step
/// leaves 16 KiB overlap context so lines straddling page boundaries remain easy to read).
pub const PAGE_STEP_BYTES: u64 = 48 * 1024;

/// Maximum characters rendered on one visual line row in Paged View before wrapping into a
/// continuation segment (`↪`). Keeps CoreText/GPUI shaping strictly bounded even on a 128 MiB
/// single-line file (`L03`) without counting continuation wraps as logical file lines (`§6.4`).
pub const MAX_DISPLAY_SEGMENT_CHARS: usize = 180;

#[derive(Clone, Debug)]
pub struct DisplaySegment {
    /// 1-based logical file line number (only on the first segment of a logical line, when known).
    pub logical_line_1based: Option<u64>,
    /// Exact raw file byte range represented by this visual segment when `!slice.lossy`.
    /// `None` when `slice.lossy == true` (invalid UTF-8 bytes expand to 3-byte `\u{FFFD}`,
    /// so decoded string byte offsets do not map 1:1 to raw file byte offsets).
    pub exact_file_byte_range: Option<Range<u64>>,
    /// Whether this row is a continuation fragment of a long line (`↪`), NOT a new logical line.
    pub is_continuation: bool,
    /// Slice into `ViewportSlice::text` (`char`-boundary aligned).
    pub text_range: Range<usize>,
}

/// Pure segmenter that splits a `<= 64 KiB` `ViewportSlice` into bounded display rows without
/// allocating per-line `String`s.
pub fn build_display_segments(slice: &ViewportSlice) -> Vec<DisplaySegment> {
    let text: &str = &slice.text;
    if text.is_empty() {
        return vec![DisplaySegment {
            logical_line_1based: slice.first_line_number.map(|n| n + 1),
            exact_file_byte_range: (!slice.lossy)
                .then_some(slice.byte_range.start..slice.byte_range.start),
            is_continuation: false,
            text_range: 0..0,
        }];
    }

    let mut segments = Vec::with_capacity(64);
    let mut current_logical_line = slice.first_line_number.map(|n| n + 1);
    let base_file_byte = slice.byte_range.start;
    let max_file_byte = slice.byte_range.end;
    let map_exact_range = |abs_start: usize, abs_end: usize| -> Option<Range<u64>> {
        (!slice.lossy).then(|| {
            let s = (base_file_byte + abs_start as u64).min(max_file_byte);
            let e = (base_file_byte + abs_end as u64).min(max_file_byte);
            s..e
        })
    };
    let bytes = text.as_bytes();
    let mut line_start = 0usize;

    while line_start < bytes.len() {
        let newline_rel = memchr_newline(&bytes[line_start..]);
        let (content_end, next_line_start, has_newline) = match newline_rel {
            Some(rel) => {
                let nl_pos = line_start + rel;
                let content_end = if nl_pos > line_start && bytes[nl_pos - 1] == b'\r' {
                    nl_pos - 1
                } else {
                    nl_pos
                };
                (content_end, nl_pos + 1, true)
            }
            None => (bytes.len(), bytes.len(), false),
        };

        if content_end == line_start {
            segments.push(DisplaySegment {
                logical_line_1based: current_logical_line,
                exact_file_byte_range: map_exact_range(line_start, line_start),
                is_continuation: false,
                text_range: line_start..line_start,
            });
        } else {
            let line_str = &text[line_start..content_end];
            let mut seg_start_in_line = 0usize;
            let mut char_count = 0usize;
            let mut first_seg = true;

            for (byte_idx, _) in line_str.char_indices() {
                if char_count == MAX_DISPLAY_SEGMENT_CHARS {
                    let abs_start = line_start + seg_start_in_line;
                    let abs_end = line_start + byte_idx;
                    segments.push(DisplaySegment {
                        logical_line_1based: if first_seg {
                            current_logical_line
                        } else {
                            None
                        },
                        exact_file_byte_range: map_exact_range(abs_start, abs_end),
                        is_continuation: !first_seg,
                        text_range: abs_start..abs_end,
                    });
                    seg_start_in_line = byte_idx;
                    char_count = 0;
                    first_seg = false;
                }
                char_count += 1;
            }

            let abs_start = line_start + seg_start_in_line;
            let abs_end = content_end;
            segments.push(DisplaySegment {
                logical_line_1based: if first_seg {
                    current_logical_line
                } else {
                    None
                },
                exact_file_byte_range: map_exact_range(abs_start, abs_end),
                is_continuation: !first_seg,
                text_range: abs_start..abs_end,
            });
        }

        if has_newline {
            if let Some(ln) = &mut current_logical_line {
                *ln = ln.saturating_add(1);
            }
        }
        line_start = next_line_start;
    }

    segments
}

#[inline]
fn memchr_newline(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == b'\n')
}

/// State of a read-only Paged Large-File (or admission-budget-routed) tab (`§6`, `B5`).
pub struct PagedTabState {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub meta: DocMeta,
    pub handle: PagedDocHandle,
    pub source_gen: SourceGeneration,
    pub readonly_reason: ReadOnlyReason,
    /// Present when an ordinary-sized disk file was routed to Paged View due to the 256 MiB /
    /// 128-editor hydrated admission budget (`§15.2`).
    pub admission_note: Option<String>,

    pub current_anchor_byte: u64,
    pub current_slice: Option<ViewportSlice>,
    pub cached_segments: Arc<Vec<DisplaySegment>>,
    pub segment_item_sizes: Rc<Vec<Size<Pixels>>>,
    pub scroll_handle: VirtualListScrollHandle,
    pub index_status: IndexStatus,
    pub highlighted_byte_range: Option<Range<u64>>,
    pub source_error: Option<String>,
    pub is_loading: bool,

    viewport_req_seq: u64,
    viewport_cancel: Option<CancelToken>,
    index_cancel: CancelToken,
}

impl Drop for PagedTabState {
    fn drop(&mut self) {
        if let Some(cancel) = self.viewport_cancel.take() {
            cancel.cancel();
        }
        self.index_cancel.cancel();
    }
}

impl PagedTabState {
    pub fn new(
        doc_id: DocumentId,
        lifecycle_gen: LifecycleGen,
        mut meta: DocMeta,
        handle: PagedDocHandle,
        source_gen: SourceGeneration,
        readonly_reason: ReadOnlyReason,
        admission_note: Option<String>,
        initial_byte_offset: u64,
    ) -> Self {
        meta.paged = true;
        meta.readonly_reason = Some(readonly_reason.clone());
        let index_status = handle.index_status();
        let clamped_offset = initial_byte_offset.min(handle.len());

        Self {
            doc_id,
            lifecycle_gen,
            meta,
            handle,
            source_gen,
            readonly_reason,
            admission_note,
            current_anchor_byte: clamped_offset,
            current_slice: None,
            cached_segments: Arc::new(Vec::new()),
            segment_item_sizes: Rc::new(Vec::new()),
            scroll_handle: VirtualListScrollHandle::new(),
            index_status,
            highlighted_byte_range: None,
            source_error: None,
            is_loading: false,
            viewport_req_seq: 0,
            viewport_cancel: None,
            index_cancel: CancelToken::new(),
        }
    }

    pub fn index_cancel_token(&self) -> CancelToken {
        self.index_cancel.clone()
    }

    pub fn prepare_viewport_request(
        &mut self,
        anchor: ViewportAnchor,
    ) -> (u64, ViewportRequest, CancelToken) {
        if let Some(prev) = self.viewport_cancel.take() {
            prev.cancel();
        }
        self.viewport_req_seq = self.viewport_req_seq.wrapping_add(1);
        let cancel = CancelToken::new();
        self.viewport_cancel = Some(cancel.clone());
        self.is_loading = true;
        let req = ViewportRequest {
            source_gen: self.source_gen,
            anchor,
            max_bytes: VIEWPORT_BYTES,
        };
        (self.viewport_req_seq, req, cancel)
    }

    pub fn apply_viewport_result(
        &mut self,
        seq: u64,
        result: Result<ViewportSlice, editor_services::ServiceError>,
    ) -> bool {
        if seq != self.viewport_req_seq {
            return false;
        }
        self.is_loading = false;
        match result {
            Ok(slice) => {
                if slice.source_gen != self.source_gen {
                    self.source_error = Some(
                        "Underlying file changed on disk; reload to inspect updated content."
                            .to_string(),
                    );
                    return true;
                }
                self.current_anchor_byte = slice.byte_range.start;
                self.index_status = slice.index_status;
                let segs = build_display_segments(&slice);
                let row_size = size(px(1600.), px(20.));
                self.segment_item_sizes = Rc::new(vec![row_size; segs.len()]);
                self.cached_segments = Arc::new(segs);
                self.scroll_handle.scroll_to_item(0, ScrollStrategy::Top);
                self.current_slice = Some(slice);
                self.source_error = None;
                true
            }
            Err(err) => {
                if err.kind == editor_services::ErrorKind::Cancelled {
                    return false;
                }
                self.source_error = Some(err.message);
                true
            }
        }
    }

    pub fn total_bytes(&self) -> u64 {
        self.handle.len()
    }

    pub fn visible_byte_range(&self) -> Range<u64> {
        self.current_slice
            .as_ref()
            .map(|s| s.byte_range.clone())
            .unwrap_or(self.current_anchor_byte..self.current_anchor_byte)
    }

    pub fn next_page_offset(&self) -> u64 {
        if let Some(slice) = &self.current_slice {
            if slice.byte_range.end >= self.total_bytes() {
                return slice.byte_range.start;
            }
            (slice.byte_range.start + PAGE_STEP_BYTES).min(self.total_bytes().saturating_sub(1))
        } else {
            (self.current_anchor_byte + PAGE_STEP_BYTES).min(self.total_bytes())
        }
    }

    pub fn prev_page_offset(&self) -> u64 {
        self.current_anchor_byte.saturating_sub(PAGE_STEP_BYTES)
    }

    pub fn end_page_offset(&self) -> u64 {
        self.total_bytes().saturating_sub(VIEWPORT_BYTES as u64 / 2)
    }

    pub fn capture_view_state(&self) -> ViewState {
        ViewState {
            cursor_byte: self.current_anchor_byte,
            scroll_byte: self.current_anchor_byte,
        }
    }

    pub fn reason_banner(&self) -> String {
        if let Some(note) = &self.admission_note {
            return note.clone();
        }
        match self.readonly_reason {
            ReadOnlyReason::LargeFile => format!(
                "Paged Read-Only Mode ({}) — bounded 64 KiB viewport & global 64 MiB cache; full-file Editor load disabled.",
                format_bytes(self.total_bytes())
            ),
            ReadOnlyReason::LongLine => {
                "Paged Read-Only Mode — file contains ultra-long lines (> 1 MiB); bounded 64 KiB fragment display enabled."
                    .to_string()
            }
            ReadOnlyReason::UnsupportedEncoding => {
                "Paged Read-Only Mode — non-UTF-8 / unsupported encoding; editing disabled to prevent silent data corruption."
                    .to_string()
            }
            ReadOnlyReason::MixedLineEndings => {
                "Paged Read-Only Mode — mixed LF/CRLF/CR line endings; editing disabled to preserve exact bytes."
                    .to_string()
            }
            ReadOnlyReason::BinaryContent => {
                "Paged Read-Only Mode — binary / NUL-byte content detected; bounded lossy preview shown."
                    .to_string()
            }
        }
    }

    pub fn encoding_label(&self) -> &'static str {
        match self.meta.encoding {
            EncodingSpec::Utf8 => "UTF-8 (prefix)",
            EncodingSpec::Utf8Bom => "UTF-8 BOM",
            EncodingSpec::UnsupportedReadOnly => "Non-UTF-8 [RO]",
        }
    }

    pub fn eol_label(&self) -> &'static str {
        match self.meta.line_ending {
            LineEnding::Lf => "LF (prefix)",
            LineEnding::Crlf => "CRLF (prefix)",
            LineEnding::MixedReadOnly => "Mixed EOL [RO]",
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GiB", b / GIB)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", b / MIB)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Parses a user jump input:
/// - `"b:4096"`, `"@4096"`, `"0x1000"` -> `ByteOffset(u64)` (full 64-bit > 4 GiB supported)
/// - `"l:120"`, `":120"` -> `ApproxLine(119)` (1-based user line -> 0-based `ViewportAnchor::ApproxLine`)
/// - Plain integer `"500"`: in Paged mode defaults to byte offset if `prefer_bytes` is true, else 1-based line!
pub fn parse_jump_query(raw: &str, prefer_bytes: bool) -> Result<ViewportAnchor, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter a byte offset (e.g. b:1048576 or 0x100000) or line (:120)".to_string());
    }

    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        let clean = hex.replace('_', "");
        let byte = u64::from_str_radix(&clean, 16)
            .map_err(|_| format!("Invalid hex byte offset '{trimmed}'"))?;
        return Ok(ViewportAnchor::ByteOffset(byte));
    }

    if let Some(rest) = trimmed
        .strip_prefix("b:")
        .or_else(|| trimmed.strip_prefix("B:"))
        .or_else(|| trimmed.strip_prefix('@'))
    {
        let clean = rest.trim().replace('_', "");
        let byte: u64 = clean
            .parse()
            .map_err(|_| format!("Invalid 64-bit byte offset '{rest}'"))?;
        return Ok(ViewportAnchor::ByteOffset(byte));
    }

    if let Some(rest) = trimmed
        .strip_prefix("l:")
        .or_else(|| trimmed.strip_prefix("L:"))
        .or_else(|| trimmed.strip_prefix(':'))
    {
        let clean = rest.trim().replace('_', "");
        let line_1based: u64 = clean
            .parse()
            .map_err(|_| format!("Invalid line number '{rest}'"))?;
        if line_1based == 0 {
            return Err("Line numbers are 1-based (>= 1)".to_string());
        }
        return Ok(ViewportAnchor::ApproxLine(line_1based - 1));
    }

    let clean = trimmed.replace('_', "");
    let value: u64 = clean.parse().map_err(|_| {
        format!("Invalid jump target '{trimmed}'; use b:<byte>, 0x<hex>, or :<line>")
    })?;
    if prefer_bytes {
        Ok(ViewportAnchor::ByteOffset(value))
    } else {
        if value == 0 {
            return Err("Line numbers are 1-based (>= 1); use b:0 for byte 0".to_string());
        }
        Ok(ViewportAnchor::ApproxLine(value - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_long_single_line_without_advancing_logical_line_number() {
        let long_text: Arc<str> = Arc::from("x".repeat(MAX_DISPLAY_SEGMENT_CHARS * 2 + 25));
        let slice = ViewportSlice {
            source_gen: SourceGeneration(1),
            byte_range: 1000..(1000 + long_text.len() as u64),
            text: long_text,
            first_line_number: Some(41), // 0-based -> line 42
            is_line_fragment: true,
            lossy: false,
            index_status: IndexStatus::Complete {
                total_bytes: 5000,
                total_lines: 50,
            },
        };

        let segs = build_display_segments(&slice);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0].logical_line_1based, Some(42));
        assert!(!segs[0].is_continuation);
        assert_eq!(segs[1].logical_line_1based, None);
        assert!(segs[1].is_continuation);
        assert_eq!(segs[2].logical_line_1based, None);
        assert!(segs[2].is_continuation);
        assert_eq!(
            segs[2].exact_file_byte_range,
            Some(
                (1000 + (MAX_DISPLAY_SEGMENT_CHARS * 2) as u64)
                    ..(1000 + (MAX_DISPLAY_SEGMENT_CHARS * 2 + 25) as u64)
            )
        );
    }

    #[test]
    fn lossy_viewport_slice_never_claims_exact_row_file_offsets() {
        // Simulate 4 invalid bytes expanding to 4 * 3 = 12 UTF-8 bytes of '\u{FFFD}' + "\nabc"
        let lossy_text: Arc<str> = Arc::from("\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}\nabc");
        let slice = ViewportSlice {
            source_gen: SourceGeneration(2),
            byte_range: 500..508, // raw file had only 8 bytes, while decoded text has 16 bytes!
            text: lossy_text,
            first_line_number: Some(0),
            is_line_fragment: false,
            lossy: true,
            index_status: IndexStatus::Complete {
                total_bytes: 508,
                total_lines: 2,
            },
        };

        let segs = build_display_segments(&slice);
        assert_eq!(segs.len(), 2);
        assert!(segs.iter().all(|s| s.exact_file_byte_range.is_none()));
    }

    #[test]
    fn parses_64bit_byte_offsets_above_4gib() {
        let anchor = parse_jump_query("b:5368709120", false).expect("5 GiB offset");
        assert!(matches!(anchor, ViewportAnchor::ByteOffset(5_368_709_120)));

        let hex_anchor = parse_jump_query("0x1_4000_0000", true).expect("5 GiB hex");
        assert!(matches!(
            hex_anchor,
            ViewportAnchor::ByteOffset(0x1_4000_0000)
        ));

        let line_anchor = parse_jump_query(":250", true).expect("line 250");
        assert!(matches!(line_anchor, ViewportAnchor::ApproxLine(249)));
    }

    #[test]
    fn segments_dense_64kib_short_lines_without_allocating_strings() {
        let dense_text: Arc<str> = Arc::from("a\n".repeat(32_768));
        let slice = ViewportSlice {
            source_gen: SourceGeneration(3),
            byte_range: 0..65_536,
            text: dense_text,
            first_line_number: Some(0),
            is_line_fragment: false,
            lossy: false,
            index_status: IndexStatus::Complete {
                total_bytes: 65_536,
                total_lines: 32_768,
            },
        };

        let segs = build_display_segments(&slice);
        // "a\n" repeated 32_768 times ends with '\n', so line_start reaches bytes.len() cleanly.
        assert_eq!(segs.len(), 32_768);
        assert_eq!(segs[0].logical_line_1based, Some(1));
        assert_eq!(segs[32_767].logical_line_1based, Some(32_768));
    }
}
