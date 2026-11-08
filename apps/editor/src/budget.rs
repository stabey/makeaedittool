use crate::editor_adapter::MAX_ORDINARY_DOC_BYTES;
use crate::paged_view::format_bytes;
use editor_services::DocumentId;

/// Explicit ordinary-editor hydration admission budget (`§15.2`, `ADR-005`).
///
/// Important: `max_hydrated_bytes` bounds admitted logical UTF-8 `Rope` bytes across live
/// `EditorState` tabs — it is NOT a hard OS resident-set (RSS) cap because undo history and
/// shaped line layouts hold additional memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HydrationBudgetConfig {
    pub max_hydrated_bytes: u64,
    pub soft_clean_editor_target: usize,
    pub max_hydrated_editors: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HydratedSlotInfo {
    pub doc_id: DocumentId,
    pub byte_len: u64,
    pub is_dirty: bool,
    pub has_undo_history: bool,
    pub is_file_backed: bool,
    pub is_active: bool,
    pub last_activated_tick: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdmissionDecision {
    Admit {
        dehydrate_documents: Vec<DocumentId>,
    },
    OverBudget {
        reason: String,
        hydrated_bytes: u64,
        hydrated_editors: usize,
    },
}

/// Computes whether a candidate ordinary document of `incoming_bytes` can be hydrated into a live
/// `EditorState`, optionally reclaiming LRU inactive tabs that have **neither** unsaved changes
/// (`!is_dirty`) **nor** session undo history (`!has_undo_history`).
///
/// Dirty tabs and edited tabs with undo history are NEVER evicted.
pub fn plan_hydration_admission(
    config: HydrationBudgetConfig,
    current_hydrated: &[HydratedSlotInfo],
    incoming_bytes: u64,
) -> AdmissionDecision {
    if incoming_bytes > MAX_ORDINARY_DOC_BYTES as u64 {
        let total_bytes: u64 = current_hydrated.iter().map(|s| s.byte_len).sum();
        return AdmissionDecision::OverBudget {
            reason: format!(
                "Document size ({}) exceeds single ordinary-editor limit ({})",
                format_bytes(incoming_bytes),
                format_bytes(MAX_ORDINARY_DOC_BYTES as u64)
            ),
            hydrated_bytes: total_bytes,
            hydrated_editors: current_hydrated.len(),
        };
    }

    let mut remaining_bytes: u64 = current_hydrated.iter().map(|s| s.byte_len).sum();
    let mut remaining_editors: usize = current_hydrated.len();

    let needs_clean_reclaim = remaining_editors >= config.soft_clean_editor_target
        || remaining_editors >= config.max_hydrated_editors
        || remaining_bytes.saturating_add(incoming_bytes) > config.max_hydrated_bytes;

    let mut dehydrate_documents = Vec::new();

    if needs_clean_reclaim {
        let mut reclaimable: Vec<HydratedSlotInfo> = current_hydrated
            .iter()
            .copied()
            .filter(|slot| {
                !slot.is_active && !slot.is_dirty && !slot.has_undo_history && slot.is_file_backed
            })
            .collect();
        reclaimable.sort_by_key(|slot| slot.last_activated_tick);

        for candidate in reclaimable {
            let over_soft_count = remaining_editors >= config.soft_clean_editor_target;
            let over_hard_count = remaining_editors >= config.max_hydrated_editors;
            let over_bytes =
                remaining_bytes.saturating_add(incoming_bytes) > config.max_hydrated_bytes;
            if !over_soft_count && !over_hard_count && !over_bytes {
                break;
            }
            dehydrate_documents.push(candidate.doc_id);
            remaining_bytes = remaining_bytes.saturating_sub(candidate.byte_len);
            remaining_editors = remaining_editors.saturating_sub(1);
        }
    }

    if remaining_editors >= config.max_hydrated_editors {
        return AdmissionDecision::OverBudget {
            reason: format!(
                "Hydrated editor entity cap reached ({} / {} active dirty/edited editors); refusing silent eviction of unsaved drafts or undo history.",
                remaining_editors, config.max_hydrated_editors
            ),
            hydrated_bytes: remaining_bytes,
            hydrated_editors: remaining_editors,
        };
    }

    if remaining_bytes.saturating_add(incoming_bytes) > config.max_hydrated_bytes {
        return AdmissionDecision::OverBudget {
            reason: format!(
                "Hydrated content admission budget exceeded ({} + {} > {} logical UTF-8 cap across {} live editors); dirty drafts and undo history are never evicted.",
                format_bytes(remaining_bytes),
                format_bytes(incoming_bytes),
                format_bytes(config.max_hydrated_bytes),
                remaining_editors
            ),
            hydrated_bytes: remaining_bytes,
            hydrated_editors: remaining_editors,
        };
    }

    AdmissionDecision::Admit {
        dehydrate_documents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_100_edited_small_tabs_without_evicting_dirty_or_undo_history() {
        let config = HydrationBudgetConfig {
            max_hydrated_bytes: 256 * 1024 * 1024,
            soft_clean_editor_target: 32,
            max_hydrated_editors: 128,
        };

        let existing: Vec<HydratedSlotInfo> = (0..99)
            .map(|i| HydratedSlotInfo {
                doc_id: DocumentId(i),
                byte_len: 4096,
                is_dirty: i % 2 == 0,
                has_undo_history: true,
                is_file_backed: true,
                is_active: i == 98,
                last_activated_tick: i as u64,
            })
            .collect();

        let decision = plan_hydration_admission(config, &existing, 4096);
        assert_eq!(
            decision,
            AdmissionDecision::Admit {
                dehydrate_documents: Vec::new()
            }
        );
    }

    #[test]
    fn reclaims_clean_unedited_lru_tabs_past_soft_target_preserving_edited_tabs() {
        let config = HydrationBudgetConfig {
            max_hydrated_bytes: 256 * 1024 * 1024,
            soft_clean_editor_target: 4,
            max_hydrated_editors: 128,
        };

        let existing = vec![
            HydratedSlotInfo {
                doc_id: DocumentId(0),
                byte_len: 1024,
                is_dirty: true,
                has_undo_history: true,
                is_file_backed: true,
                is_active: false,
                last_activated_tick: 1,
            },
            HydratedSlotInfo {
                doc_id: DocumentId(1),
                byte_len: 1024,
                is_dirty: false,
                has_undo_history: false,
                is_file_backed: true,
                is_active: false,
                last_activated_tick: 2,
            },
            HydratedSlotInfo {
                doc_id: DocumentId(2),
                byte_len: 1024,
                is_dirty: false,
                has_undo_history: true, // Edited then undone -> still has undo/redo history; NEVER evict!
                is_file_backed: true,
                is_active: false,
                last_activated_tick: 3,
            },
            HydratedSlotInfo {
                doc_id: DocumentId(3),
                byte_len: 1024,
                is_dirty: false,
                has_undo_history: false,
                is_file_backed: true,
                is_active: true,
                last_activated_tick: 4,
            },
        ];

        let decision = plan_hydration_admission(config, &existing, 1024);
        assert_eq!(
            decision,
            AdmissionDecision::Admit {
                dehydrate_documents: vec![DocumentId(1)]
            }
        );
    }

    #[test]
    fn rejects_over_256mib_when_dirty_tabs_hold_budget() {
        let config = HydrationBudgetConfig {
            max_hydrated_bytes: 256 * 1024 * 1024,
            soft_clean_editor_target: 32,
            max_hydrated_editors: 128,
        };

        let four_dirty_64m: Vec<HydratedSlotInfo> = (0..4)
            .map(|i| HydratedSlotInfo {
                doc_id: DocumentId(i),
                byte_len: 64 * 1024 * 1024,
                is_dirty: true,
                has_undo_history: true,
                is_file_backed: false,
                is_active: i == 3,
                last_activated_tick: i as u64,
            })
            .collect();

        let decision = plan_hydration_admission(config, &four_dirty_64m, 1024 * 1024);
        assert!(matches!(decision, AdmissionDecision::OverBudget { .. }));
    }

    #[test]
    fn untouched_pathless_untitled_tabs_are_not_counted_reclaimable_past_hard_cap() {
        let config = HydrationBudgetConfig {
            max_hydrated_bytes: 256 * 1024 * 1024,
            soft_clean_editor_target: 2,
            max_hydrated_editors: 4,
        };

        // 4 untouched untitled tabs (is_file_backed = false) occupy the hard cap of 4 editors.
        let four_untitled: Vec<HydratedSlotInfo> = (0..4)
            .map(|i| HydratedSlotInfo {
                doc_id: DocumentId(i),
                byte_len: 0,
                is_dirty: false,
                has_undo_history: false,
                is_file_backed: false,
                is_active: i == 3,
                last_activated_tick: i as u64,
            })
            .collect();

        let decision = plan_hydration_admission(config, &four_untitled, 1024);
        assert!(matches!(decision, AdmissionDecision::OverBudget { .. }));
    }
}
