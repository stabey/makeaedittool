use std::ops::Range;

use editor_services::LineEnding;
use ropey::{LineType, Rope};

use crate::editor_adapter::{MAX_BOUNDED_SPAN_BYTES, MAX_ORDINARY_DOC_BYTES, splits_crlf};
use crate::language::comment_prefix_for_language;

/// Maximum byte span or output size materialized by a single line command (`4 MiB`).
pub const MAX_LINE_OP_BYTES: usize = MAX_BOUNDED_SPAN_BYTES;

/// Maximum number of logical lines inspected or mutated by a single line command.
pub const MAX_LINE_OP_LINES: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineOpKind {
    Duplicate,
    Delete,
    MoveUp,
    MoveDown,
    ToggleComment { prefix: &'static str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineOpError {
    MultipleSelectionsUnsupported,
    UnsupportedCommentLanguage { language_label: String },
    LineCountExceeded { lines: usize, max_lines: usize },
    ByteBudgetExceeded { bytes: usize, max_bytes: usize },
}

impl std::fmt::Display for LineOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MultipleSelectionsUnsupported => write!(
                f,
                "Multiple selections are active; custom line commands operate on a single active selection."
            ),
            Self::UnsupportedCommentLanguage { language_label } => write!(
                f,
                "Line comments are not supported for {language_label} (supported for //, #, and -- languages only)."
            ),
            Self::LineCountExceeded { lines, max_lines } => write!(
                f,
                "Selection spans {lines} lines, exceeding the {max_lines}-line single-command safety cap."
            ),
            Self::ByteBudgetExceeded { bytes, max_bytes } => write!(
                f,
                "Line command span ({bytes} B) exceeds the {max_bytes} B UI atomic edit budget."
            ),
        }
    }
}

impl std::error::Error for LineOpError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineOpPlan {
    NoOp,
    Apply {
        edits: Vec<(Range<usize>, String)>,
        new_selection: Range<usize>,
    },
}

/// Resolves and plans `ToggleComment` for `language_id`/`language_label`, returning
/// `LineOpError::UnsupportedCommentLanguage` for formats without line-comment syntax.
pub fn plan_toggle_comment_for_language(
    rope: &Rope,
    selection: Range<usize>,
    line_ending: LineEnding,
    has_secondary_nonempty_selection: bool,
    language_id: &str,
    language_label: &str,
) -> Result<LineOpPlan, LineOpError> {
    let Some(prefix) = comment_prefix_for_language(language_id) else {
        return Err(LineOpError::UnsupportedCommentLanguage {
            language_label: language_label.to_string(),
        });
    };
    plan_line_op(
        rope,
        selection,
        line_ending,
        has_secondary_nonempty_selection,
        LineOpKind::ToggleComment { prefix },
    )
}

/// Pure, zero-UI-allocation-on-reject planner for active-selection line commands.
///
/// Guarantees:
/// - Rejects detectable multiple selections (`has_secondary_nonempty_selection`).
/// - Preserves UTF-8 character boundaries and never splits `\r\n` pairs.
/// - Excludes `end_line` when a non-empty selection ends at column 0 of `end_line`.
/// - Handles first line, middle lines, and EOF both with and without a trailing newline.
/// - Enforces `MAX_LINE_OP_LINES` and `MAX_LINE_OP_BYTES` (4 MiB) BEFORE allocating strings.
pub fn plan_line_op(
    rope: &Rope,
    selection: Range<usize>,
    line_ending: LineEnding,
    has_secondary_nonempty_selection: bool,
    kind: LineOpKind,
) -> Result<LineOpPlan, LineOpError> {
    if has_secondary_nonempty_selection {
        return Err(LineOpError::MultipleSelectionsUnsupported);
    }

    let sel = normalize_selection(rope, selection);
    let total_lines = rope.len_lines(LineType::LF).max(1);
    let first_line = rope.byte_to_line_idx(sel.start, LineType::LF);
    let raw_end_line = rope.byte_to_line_idx(sel.end, LineType::LF);
    let raw_end_line_start = rope.line_to_byte_idx(raw_end_line, LineType::LF);

    let last_line =
        if sel.end > sel.start && sel.end == raw_end_line_start && raw_end_line > first_line {
            raw_end_line - 1
        } else {
            raw_end_line
        };

    let selected_line_count = last_line - first_line + 1;
    if selected_line_count > MAX_LINE_OP_LINES {
        return Err(LineOpError::LineCountExceeded {
            lines: selected_line_count,
            max_lines: MAX_LINE_OP_LINES,
        });
    }

    let block_start = rope.line_to_byte_idx(first_line, LineType::LF);
    let block_raw_end = line_end_byte(rope, last_line, total_lines);
    let block_content_end = strip_line_break_end(rope, block_start, block_raw_end);

    match kind {
        LineOpKind::Duplicate => {
            let default_sep = default_eol_str(line_ending);
            let sep = if block_content_end < block_raw_end {
                if block_raw_end - block_content_end == 2 {
                    "\r\n"
                } else {
                    "\n"
                }
            } else {
                default_sep
            };

            let content_len = block_content_end - block_start;
            let insert_len = sep.len().saturating_add(content_len);
            check_byte_budget(insert_len)?;
            if rope.len().saturating_add(insert_len) > MAX_ORDINARY_DOC_BYTES {
                return Err(LineOpError::ByteBudgetExceeded {
                    bytes: rope.len().saturating_add(insert_len),
                    max_bytes: MAX_ORDINARY_DOC_BYTES,
                });
            }

            let mut inserted = String::with_capacity(insert_len);
            inserted.push_str(sep);
            append_rope_range(&mut inserted, rope, block_start..block_content_end);

            let new_selection = if sel.start == sel.end {
                let pos = sel.start.min(block_content_end) + insert_len;
                pos..pos
            } else if sel.start == block_start
                && sel.end == block_raw_end
                && block_raw_end > block_content_end
            {
                (block_start + insert_len)..(block_raw_end + insert_len)
            } else {
                (sel.start + insert_len)..(sel.end.min(block_content_end) + insert_len)
            };

            Ok(LineOpPlan::Apply {
                edits: vec![(block_content_end..block_content_end, inserted)],
                new_selection,
            })
        }

        LineOpKind::Delete => {
            if rope.len() == 0 {
                return Ok(LineOpPlan::NoOp);
            }

            let (delete_range, target_cursor) = if block_content_end < block_raw_end {
                // Selected block ends with a line break; remove `block_start..block_raw_end`.
                (block_start..block_raw_end, block_start)
            } else if first_line > 0 {
                // Selected block reaches EOF without a trailing newline (or is the empty trailing line):
                // consume the preceding line break so no dangling blank line is left behind.
                let prev_line_start = rope.line_to_byte_idx(first_line - 1, LineType::LF);
                let prev_content_end = strip_line_break_end(rope, prev_line_start, block_start);
                (prev_content_end..block_raw_end, prev_line_start)
            } else {
                (0..block_raw_end, 0)
            };

            if delete_range.is_empty() {
                return Ok(LineOpPlan::NoOp);
            }
            check_byte_budget(delete_range.len())?;

            let new_doc_len = rope.len() - delete_range.len();
            let clamped_cursor = target_cursor.min(new_doc_len);
            Ok(LineOpPlan::Apply {
                edits: vec![(delete_range, String::new())],
                new_selection: clamped_cursor..clamped_cursor,
            })
        }

        LineOpKind::MoveUp => {
            if first_line == 0 {
                return Ok(LineOpPlan::NoOp);
            }

            let prev_line = first_line - 1;
            let prev_start = rope.line_to_byte_idx(prev_line, LineType::LF);
            let prev_content_end = strip_line_break_end(rope, prev_start, block_start);
            let prev_sep = if block_start - prev_content_end == 2 {
                "\r\n"
            } else {
                "\n"
            };

            let span_len = block_content_end - prev_start;
            check_byte_budget(span_len)?;

            let mut replacement = String::with_capacity(span_len);
            append_rope_range(&mut replacement, rope, block_start..block_content_end);
            replacement.push_str(prev_sep);
            append_rope_range(&mut replacement, rope, prev_start..prev_content_end);

            let shift = block_start - prev_start;
            let new_selection = if sel.start == block_start
                && sel.end == block_raw_end
                && block_raw_end > block_content_end
            {
                let moved_content_len = block_content_end - block_start;
                prev_start..(prev_start + moved_content_len + prev_sep.len())
            } else {
                let s = sel.start.min(block_content_end).saturating_sub(shift);
                let e = sel.end.min(block_content_end).saturating_sub(shift);
                s..e
            };

            Ok(LineOpPlan::Apply {
                edits: vec![(prev_start..block_content_end, replacement)],
                new_selection,
            })
        }

        LineOpKind::MoveDown => {
            let next_line = last_line + 1;
            if next_line >= total_lines {
                return Ok(LineOpPlan::NoOp);
            }

            let next_start = block_raw_end;
            let next_raw_end = line_end_byte(rope, next_line, total_lines);
            let next_content_end = strip_line_break_end(rope, next_start, next_raw_end);
            let mid_sep = if next_start - block_content_end == 2 {
                "\r\n"
            } else {
                "\n"
            };

            let span_len = next_content_end - block_start;
            check_byte_budget(span_len)?;

            let mut replacement = String::with_capacity(span_len);
            append_rope_range(&mut replacement, rope, next_start..next_content_end);
            replacement.push_str(mid_sep);
            append_rope_range(&mut replacement, rope, block_start..block_content_end);

            let shift = (next_content_end - next_start) + mid_sep.len();
            let new_selection = if sel.start == block_start
                && sel.end == block_raw_end
                && block_raw_end > block_content_end
            {
                (block_start + shift)..next_raw_end
            } else {
                let s = sel.start.min(block_content_end) + shift;
                let e = sel.end.min(block_content_end) + shift;
                s..e
            };

            Ok(LineOpPlan::Apply {
                edits: vec![(block_start..next_content_end, replacement)],
                new_selection,
            })
        }

        LineOpKind::ToggleComment { prefix } => {
            let span_bytes = block_content_end.saturating_sub(block_start);
            check_byte_budget(span_bytes)?;

            #[derive(Clone, Copy)]
            struct LineScan {
                indent_end: usize,
                is_blank: bool,
                has_prefix: bool,
                has_space_after_prefix: bool,
            }

            let mut scans = Vec::with_capacity(selected_line_count);
            let mut non_blank_count = 0usize;
            let mut all_non_blank_commented = true;

            for line_ix in first_line..=last_line {
                let l_start = rope.line_to_byte_idx(line_ix, LineType::LF);
                let l_raw_end = line_end_byte(rope, line_ix, total_lines);
                let l_end = strip_line_break_end(rope, l_start, l_raw_end);
                let indent_end = find_indent_end_byte(rope, l_start, l_end);
                let is_blank = indent_end == l_end;
                let (has_prefix, has_space_after_prefix) = if !is_blank {
                    check_prefix_at(rope, indent_end, l_end, prefix)
                } else {
                    (false, false)
                };
                if !is_blank {
                    non_blank_count += 1;
                    if !has_prefix {
                        all_non_blank_commented = false;
                    }
                }
                scans.push(LineScan {
                    indent_end,
                    is_blank,
                    has_prefix,
                    has_space_after_prefix,
                });
            }

            let uncomment = non_blank_count > 0 && all_non_blank_commented;
            let insert_token = format!("{prefix} ");
            let mut edits: Vec<(Range<usize>, String)> = Vec::with_capacity(selected_line_count);
            let mut net_delta: isize = 0;

            if uncomment {
                for scan in &scans {
                    if scan.has_prefix {
                        let remove_len = prefix.len() + usize::from(scan.has_space_after_prefix);
                        edits.push((
                            scan.indent_end..(scan.indent_end + remove_len),
                            String::new(),
                        ));
                        net_delta -= remove_len as isize;
                    }
                }
            } else if non_blank_count == 0 {
                // All selected lines are blank/whitespace: comment every selected line at indent_end.
                let projected_add = insert_token.len().saturating_mul(scans.len());
                check_byte_budget(span_bytes.saturating_add(projected_add))?;
                if rope.len().saturating_add(projected_add) > MAX_ORDINARY_DOC_BYTES {
                    return Err(LineOpError::ByteBudgetExceeded {
                        bytes: rope.len().saturating_add(projected_add),
                        max_bytes: MAX_ORDINARY_DOC_BYTES,
                    });
                }
                for scan in &scans {
                    edits.push((scan.indent_end..scan.indent_end, insert_token.clone()));
                    net_delta += insert_token.len() as isize;
                }
            } else {
                let projected_add = insert_token.len().saturating_mul(non_blank_count);
                check_byte_budget(span_bytes.saturating_add(projected_add))?;
                if rope.len().saturating_add(projected_add) > MAX_ORDINARY_DOC_BYTES {
                    return Err(LineOpError::ByteBudgetExceeded {
                        bytes: rope.len().saturating_add(projected_add),
                        max_bytes: MAX_ORDINARY_DOC_BYTES,
                    });
                }
                for scan in &scans {
                    if !scan.is_blank {
                        edits.push((scan.indent_end..scan.indent_end, insert_token.clone()));
                        net_delta += insert_token.len() as isize;
                    }
                }
            }

            if edits.is_empty() {
                return Ok(LineOpPlan::NoOp);
            }

            let projected_span = span_bytes.saturating_add_signed(net_delta);
            check_byte_budget(projected_span)?;

            let new_selection = map_selection_through_edits(sel, &edits);
            Ok(LineOpPlan::Apply {
                edits,
                new_selection,
            })
        }
    }
}

#[inline]
fn check_byte_budget(bytes: usize) -> Result<(), LineOpError> {
    if bytes > MAX_LINE_OP_BYTES {
        Err(LineOpError::ByteBudgetExceeded {
            bytes,
            max_bytes: MAX_LINE_OP_BYTES,
        })
    } else {
        Ok(())
    }
}

#[inline]
fn default_eol_str(line_ending: LineEnding) -> &'static str {
    match line_ending {
        LineEnding::Crlf => "\r\n",
        LineEnding::Lf | LineEnding::MixedReadOnly => "\n",
    }
}

fn normalize_selection(rope: &Rope, selection: Range<usize>) -> Range<usize> {
    let doc_len = rope.len();
    let raw_start = selection.start.min(doc_len);
    let raw_end = selection.end.min(doc_len).max(raw_start);

    if raw_start == raw_end {
        let mut pos = rope.floor_char_boundary(raw_start);
        if splits_crlf(rope, pos) {
            pos = pos.saturating_sub(1);
        }
        pos..pos
    } else {
        let mut start = rope.floor_char_boundary(raw_start);
        let mut end = rope.ceil_char_boundary(raw_end);
        if splits_crlf(rope, start) {
            start = start.saturating_sub(1);
        }
        if splits_crlf(rope, end) {
            end = (end + 1).min(doc_len);
        }
        start..end
    }
}

#[inline]
fn line_end_byte(rope: &Rope, line_idx: usize, total_lines: usize) -> usize {
    if line_idx + 1 < total_lines {
        rope.line_to_byte_idx(line_idx + 1, LineType::LF)
    } else {
        rope.len()
    }
}

#[inline]
fn strip_line_break_end(rope: &Rope, line_start: usize, line_raw_end: usize) -> usize {
    if line_raw_end > line_start && rope.byte(line_raw_end - 1) == b'\n' {
        if line_raw_end - 1 > line_start && rope.byte(line_raw_end - 2) == b'\r' {
            line_raw_end - 2
        } else {
            line_raw_end - 1
        }
    } else {
        line_raw_end
    }
}

fn find_indent_end_byte(rope: &Rope, line_start: usize, line_content_end: usize) -> usize {
    let mut idx = line_start;
    while idx < line_content_end {
        let b = rope.byte(idx);
        if b == b' ' || b == b'\t' {
            idx += 1;
        } else {
            break;
        }
    }
    idx
}

fn check_prefix_at(rope: &Rope, start: usize, line_end: usize, prefix: &str) -> (bool, bool) {
    let p_bytes = prefix.as_bytes();
    if start + p_bytes.len() > line_end {
        return (false, false);
    }
    for (i, &expected) in p_bytes.iter().enumerate() {
        if rope.byte(start + i) != expected {
            return (false, false);
        }
    }
    let after = start + p_bytes.len();
    let has_space = after < line_end && rope.byte(after) == b' ';
    (true, has_space)
}

fn append_rope_range(out: &mut String, rope: &Rope, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    for chunk in rope.slice(range).chunks() {
        out.push_str(chunk);
    }
}

fn map_selection_through_edits(
    sel: Range<usize>,
    edits: &[(Range<usize>, String)],
) -> Range<usize> {
    let is_collapsed = sel.start == sel.end;
    let new_start = map_offset(sel.start, edits, true);
    if is_collapsed {
        new_start..new_start
    } else {
        // Keep selection start inclusive before a same-offset insert when multi-line selecting from column 0,
        // and advance end past any insertions within the selected range.
        let range_start = map_offset(sel.start, edits, false);
        let range_end = map_offset(sel.end, edits, true).max(range_start);
        range_start..range_end
    }
}

fn map_offset(
    offset: usize,
    edits: &[(Range<usize>, String)],
    shift_on_insert_at_offset: bool,
) -> usize {
    let mut delta: isize = 0;
    for (range, replacement) in edits {
        if range.start == range.end {
            // Pure insertion
            if range.start < offset || (shift_on_insert_at_offset && range.start == offset) {
                delta += replacement.len() as isize;
            }
        } else if range.end <= offset {
            delta += replacement.len() as isize - range.len() as isize;
        } else if range.start < offset {
            // Offset was inside deleted/replaced range: clamp to end of replacement.
            let consumed = offset - range.start;
            delta += replacement.len() as isize - consumed as isize;
            break;
        } else {
            break;
        }
    }
    offset.saturating_add_signed(delta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_adapter::validate_and_plan_edits;
    use editor_services::Revision;

    fn apply_plan_to_rope(rope: &Rope, plan: &LineOpPlan) -> String {
        match plan {
            LineOpPlan::NoOp => rope.to_string(),
            LineOpPlan::Apply { edits, .. } => {
                // Validate through EditorAdapter's real UTF-8 / CRLF / overlap validator!
                let _validated = validate_and_plan_edits(
                    rope,
                    Revision(1),
                    Revision(1),
                    true,
                    false,
                    edits,
                )
                .expect("line_ops edits must pass EditorAdapter UTF-8/CRLF/non-overlap validation");

                let mut out = rope.clone();
                let mut sorted = edits.clone();
                sorted.sort_by(|a, b| b.0.start.cmp(&a.0.start));
                for (range, text) in sorted {
                    out.remove(range.clone());
                    out.insert(range.start, &text);
                }
                out.to_string()
            }
        }
    }

    #[test]
    fn duplicate_line_handles_lf_crlf_eof_without_newline_and_next_line_col0() {
        // 1. Mid-file CRLF with full-line selection ending at next-line column 0 (excludes line 1!)
        let crlf_rope = Rope::from_str("first\r\nsecond\r\nthird");
        // "first\r\n" is 0..7; byte 7 is col 0 of "second"
        let plan = plan_line_op(
            &crlf_rope,
            0..7,
            LineEnding::Crlf,
            false,
            LineOpKind::Duplicate,
        )
        .expect("duplicate crlf col0");
        assert_eq!(
            apply_plan_to_rope(&crlf_rope, &plan),
            "first\r\nfirst\r\nsecond\r\nthird"
        );

        // 2. EOF without trailing newline in CRLF mode
        let eof_plan = plan_line_op(
            &crlf_rope,
            16..16,
            LineEnding::Crlf,
            false,
            LineOpKind::Duplicate,
        )
        .expect("duplicate crlf eof without newline");
        assert_eq!(
            apply_plan_to_rope(&crlf_rope, &eof_plan),
            "first\r\nsecond\r\nthird\r\nthird"
        );

        // 3. Single-line LF buffer without newline
        let single_rope = Rope::from_str("hello");
        let single_plan = plan_line_op(
            &single_rope,
            2..2,
            LineEnding::Lf,
            false,
            LineOpKind::Duplicate,
        )
        .expect("duplicate single line");
        assert_eq!(
            apply_plan_to_rope(&single_rope, &single_plan),
            "hello\nhello"
        );
    }

    #[test]
    fn delete_line_handles_first_middle_and_eof_with_and_without_newline_crlf() {
        let crlf_no_nl = Rope::from_str("alpha\r\nbeta\r\ngamma");

        // Delete first line ("alpha\r\n")
        let p_first = plan_line_op(
            &crlf_no_nl,
            2..2,
            LineEnding::Crlf,
            false,
            LineOpKind::Delete,
        )
        .expect("delete first");
        assert_eq!(apply_plan_to_rope(&crlf_no_nl, &p_first), "beta\r\ngamma");

        // Delete middle line ("beta\r\n") with next-line col0 selection (7..13)
        let p_mid = plan_line_op(
            &crlf_no_nl,
            7..13,
            LineEnding::Crlf,
            false,
            LineOpKind::Delete,
        )
        .expect("delete mid");
        assert_eq!(apply_plan_to_rope(&crlf_no_nl, &p_mid), "alpha\r\ngamma");

        // Delete EOF line without trailing newline ("gamma") -> removes preceding \r\n cleanly!
        let p_eof = plan_line_op(
            &crlf_no_nl,
            15..15,
            LineEnding::Crlf,
            false,
            LineOpKind::Delete,
        )
        .expect("delete eof no nl");
        assert_eq!(apply_plan_to_rope(&crlf_no_nl, &p_eof), "alpha\r\nbeta");

        // Delete EOF line WITH trailing newline ("beta\r\n" in "alpha\r\nbeta\r\n")
        let crlf_with_nl = Rope::from_str("alpha\r\nbeta\r\n");
        let p_eof_nl = plan_line_op(
            &crlf_with_nl,
            8..8,
            LineEnding::Crlf,
            false,
            LineOpKind::Delete,
        )
        .expect("delete last content line with trailing crlf");
        assert_eq!(apply_plan_to_rope(&crlf_with_nl, &p_eof_nl), "alpha\r\n");
    }

    #[test]
    fn move_line_up_and_down_preserve_crlf_and_eof_boundaries() {
        let rope = Rope::from_str("one\r\ntwo\r\nthree");

        // Top line MoveUp is NoOp
        assert_eq!(
            plan_line_op(&rope, 1..1, LineEnding::Crlf, false, LineOpKind::MoveUp).unwrap(),
            LineOpPlan::NoOp
        );
        // Bottom line MoveDown (without trailing newline) is NoOp
        assert_eq!(
            plan_line_op(&rope, 12..12, LineEnding::Crlf, false, LineOpKind::MoveDown).unwrap(),
            LineOpPlan::NoOp
        );

        // Move EOF line ("three") up -> "one\r\nthree\r\ntwo" (no trailing newline invented!)
        let up_eof =
            plan_line_op(&rope, 12..12, LineEnding::Crlf, false, LineOpKind::MoveUp).unwrap();
        let after_up = apply_plan_to_rope(&rope, &up_eof);
        assert_eq!(after_up, "one\r\nthree\r\ntwo");

        // Move "three" back down -> restores "one\r\ntwo\r\nthree"
        let rope_after = Rope::from_str(&after_up);
        let LineOpPlan::Apply { new_selection, .. } = up_eof else {
            panic!("expected Apply");
        };
        let down_again = plan_line_op(
            &rope_after,
            new_selection,
            LineEnding::Crlf,
            false,
            LineOpKind::MoveDown,
        )
        .unwrap();
        assert_eq!(
            apply_plan_to_rope(&rope_after, &down_again),
            "one\r\ntwo\r\nthree"
        );
    }

    #[test]
    fn toggle_comment_supports_rust_python_sql_indented_blocks_and_rejects_json_or_multicursor() {
        // Rust `//` indented multi-line with blank line
        let rust_src = Rope::from_str("fn demo() {\r\n    let a = 1;\r\n\r\n    let b = 2;\r\n}");
        let commented_plan = plan_toggle_comment_for_language(
            &rust_src,
            13..45,
            LineEnding::Crlf,
            false,
            "rust",
            "Rust",
        )
        .expect("rust comment");
        let commented = apply_plan_to_rope(&rust_src, &commented_plan);
        assert_eq!(
            commented,
            "fn demo() {\r\n    // let a = 1;\r\n\r\n    // let b = 2;\r\n}"
        );

        // Toggle again -> uncomments back to exact original!
        let commented_rope = Rope::from_str(&commented);
        let LineOpPlan::Apply { new_selection, .. } = commented_plan else {
            panic!("expected Apply");
        };
        let uncomment_plan = plan_toggle_comment_for_language(
            &commented_rope,
            new_selection,
            LineEnding::Crlf,
            false,
            "rust",
            "Rust",
        )
        .expect("rust uncomment");
        assert_eq!(
            apply_plan_to_rope(&commented_rope, &uncomment_plan),
            rust_src.to_string()
        );

        // Python `#` and SQL `--`
        let py = Rope::from_str("  value = 42\n");
        let py_plan =
            plan_toggle_comment_for_language(&py, 0..0, LineEnding::Lf, false, "python", "Python")
                .unwrap();
        assert_eq!(apply_plan_to_rope(&py, &py_plan), "  # value = 42\n");

        let sql = Rope::from_str("SELECT 1;");
        let sql_plan =
            plan_toggle_comment_for_language(&sql, 0..0, LineEnding::Lf, false, "sql", "SQL")
                .unwrap();
        assert_eq!(apply_plan_to_rope(&sql, &sql_plan), "-- SELECT 1;");

        // Unsupported JSON & Markdown rejected cleanly
        let json = Rope::from_str("{\"a\": 1}");
        assert!(matches!(
            plan_toggle_comment_for_language(&json, 0..0, LineEnding::Lf, false, "json", "JSON"),
            Err(LineOpError::UnsupportedCommentLanguage { .. })
        ));

        // Detectable secondary selection rejected explicitly
        assert_eq!(
            plan_line_op(&sql, 0..0, LineEnding::Lf, true, LineOpKind::Duplicate),
            Err(LineOpError::MultipleSelectionsUnsupported)
        );
    }

    #[test]
    fn enforces_4mib_and_line_count_caps_before_materialization() {
        let mut big = "x".repeat(MAX_LINE_OP_BYTES + 128);
        big.push('\n');
        let rope = Rope::from_str(&big);
        assert!(matches!(
            plan_line_op(&rope, 0..0, LineEnding::Lf, false, LineOpKind::Duplicate),
            Err(LineOpError::ByteBudgetExceeded {
                max_bytes: MAX_LINE_OP_BYTES,
                ..
            })
        ));

        let many_lines = "\n".repeat(MAX_LINE_OP_LINES + 5);
        let many_rope = Rope::from_str(&many_lines);
        assert!(matches!(
            plan_line_op(
                &many_rope,
                0..many_rope.len(),
                LineEnding::Lf,
                false,
                LineOpKind::ToggleComment { prefix: "//" },
            ),
            Err(LineOpError::LineCountExceeded {
                max_lines: MAX_LINE_OP_LINES,
                ..
            })
        ));
    }
}
