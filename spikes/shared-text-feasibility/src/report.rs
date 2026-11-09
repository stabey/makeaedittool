use std::ops::Range;

use editor_services::RecoveryCommit;
use ropey::Rope;
use serde::Serialize;
use serde_json::{Value, json};

pub const TEXT_LOG_LIMIT: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct TextObservation {
    pub byte_len: usize,
    pub text: Option<String>,
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub truncated: bool,
}

impl TextObservation {
    pub fn rope(rope: &Rope) -> Self {
        if rope.len() <= TEXT_LOG_LIMIT {
            Self {
                byte_len: rope.len(),
                text: Some(rope.to_string()),
                prefix: None,
                suffix: None,
                truncated: false,
            }
        } else {
            let prefix_end = rope.floor_char_boundary(TEXT_LOG_LIMIT / 2);
            // Rounding forward keeps the suffix at or below its byte budget.
            let mut suffix_start = rope.len() - TEXT_LOG_LIMIT / 2;
            while rope.floor_char_boundary(suffix_start) != suffix_start {
                suffix_start += 1;
            }
            Self {
                byte_len: rope.len(),
                text: None,
                prefix: Some(rope.slice(..prefix_end).to_string()),
                suffix: Some(rope.slice(suffix_start..).to_string()),
                truncated: true,
            }
        }
    }

    pub fn string(text: &str) -> Self {
        if text.len() <= TEXT_LOG_LIMIT {
            Self {
                byte_len: text.len(),
                text: Some(text.to_owned()),
                prefix: None,
                suffix: None,
                truncated: false,
            }
        } else {
            let mut prefix_end = TEXT_LOG_LIMIT / 2;
            while !text.is_char_boundary(prefix_end) {
                prefix_end -= 1;
            }
            let mut suffix_start = text.len() - TEXT_LOG_LIMIT / 2;
            while !text.is_char_boundary(suffix_start) {
                suffix_start += 1;
            }
            Self {
                byte_len: text.len(),
                text: None,
                prefix: Some(text[..prefix_end].to_owned()),
                suffix: Some(text[suffix_start..].to_owned()),
                truncated: true,
            }
        }
    }

    pub fn is(&self, expected: &str) -> bool {
        self.text.as_deref() == Some(expected)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RecoveryObservation {
    pub base_revision: u64,
    pub new_revision: u64,
    pub rope: TextObservation,
    pub deltas: Vec<DeltaObservation>,
}

impl RecoveryObservation {
    pub fn capture(commit: RecoveryCommit) -> Self {
        Self {
            base_revision: commit.base_rev.0,
            new_revision: commit.new_rev.0,
            rope: TextObservation::rope(&commit.rope),
            deltas: commit
                .deltas
                .iter()
                .map(|delta| DeltaObservation {
                    byte_range: delta.byte_range.clone(),
                    replacement: TextObservation::string(&delta.replacement),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct DeltaObservation {
    pub byte_range: Range<u64>,
    pub replacement: TextObservation,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControlObservation {
    pub control: &'static str,
    pub entity_id: String,
    pub document_id: String,
    pub native: TextObservation,
    pub committed: TextObservation,
    pub marked_utf16: Option<Range<usize>>,
    pub selected_utf16: Option<Range<usize>>,
    pub selection_bytes: Range<usize>,
    pub selection_reversed: bool,
    pub cursor_byte: usize,
    pub focused: bool,
    pub applied_soft_wrap: bool,
    pub scroll_pixels: [f32; 2],
    pub visible_rows: Option<Range<usize>>,
    pub line_height_pixels: Option<f32>,
    pub visible_revision: u64,
    pub queued_revision: u64,
    pub durable_revision: u64,
    pub dirty: bool,
    pub adapter_has_undo_history: bool,
    pub active_ime: bool,
    pub native_committed_byte_equal: bool,
    pub native_committed_rope_instance_equal: bool,
    pub pending_native_identity_predicate: bool,
    pub recovery_payload: Option<RecoveryObservation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EditObservation {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_end_byte: usize,
    pub post_snapshot: TextObservation,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventObservation {
    Input {
        sequence: u64,
        control: &'static str,
        entity_id: String,
        event: String,
    },
    Edits {
        sequence: u64,
        control: &'static str,
        entity_id: String,
        control_event_count: u64,
        edits: Vec<EditObservation>,
        revision_before_ingest: u64,
        adapter_ingested: bool,
        pending_savepoint_check: Option<Value>,
        // Native state here is the state at callback time, which may already be
        // the final replay result. Each edit's post_snapshot above is authoritative.
        at_callback_after_ingest: ControlObservation,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct FrameObservation {
    pub explicit_draw_sequence: u64,
    pub draw_returned: bool,
    pub render_count_before: u64,
    pub render_count_after: u64,
    pub draw_and_flush_ms: f64,
    pub viewport_pixels: [f32; 2],
    pub scale_factor: f32,
    pub os_window_active: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct WeakEditorObservation {
    pub scenario: String,
    pub control: &'static str,
    pub entity_id: String,
    pub weak_upgradable: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OwnershipObservation {
    pub owned_adapters: usize,
    pub owned_event_subscriptions: usize,
    pub retained_immutable_snapshots: usize,
    pub editors: Vec<WeakEditorObservation>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapshotObservation {
    pub retained: TextObservation,
    pub native_byte_equal: bool,
    pub native_rope_instance_equal: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PhaseObservation {
    pub scenario: String,
    pub phase: String,
    pub delivery: String,
    pub pre_input_draw_sequence: Option<u64>,
    pub mutation_and_instrumented_event_flush_ms: Option<f64>,
    pub after_event_flush_before_draw: Vec<ControlObservation>,
    pub frame: FrameObservation,
    pub after_draw: Vec<ControlObservation>,
    pub events: Vec<EventObservation>,
    pub ownership_after_draw: OwnershipObservation,
    pub retained_snapshot: Option<SnapshotObservation>,
    pub alias_entity_id: Option<String>,
    pub pair_native_byte_equal: Option<bool>,
    pub pair_native_rope_instance_equal: Option<bool>,
}

impl PhaseObservation {
    pub fn control(&self, label: &str) -> &ControlObservation {
        self.after_draw
            .iter()
            .find(|control| control.control == label)
            .expect("scenario requested a control it did not install")
    }

    pub fn edit_events(&self, label: &str) -> usize {
        self.events
            .iter()
            .filter(|event| {
                matches!(
                    event, EventObservation::Edits { control, .. } if *control == label
                )
            })
            .count()
    }

    pub fn publishes_ime_intermediates(&self) -> bool {
        self.events.iter().any(|event| match event {
            EventObservation::Edits {
                edits,
                at_callback_after_ingest,
                ..
            } => {
                edits
                    .iter()
                    .any(|edit| edit.post_snapshot.is("an") || edit.post_snapshot.is("ani"))
                    || at_callback_after_ingest.committed.is("an")
                    || at_callback_after_ingest.committed.is("ani")
                    || at_callback_after_ingest
                        .recovery_payload
                        .as_ref()
                        .is_some_and(|payload| payload.rope.is("an") || payload.rope.is("ani"))
            }
            EventObservation::Input { .. } => false,
        })
    }
}

#[derive(Serialize)]
pub struct Expectation {
    pub scenario: String,
    pub phase: String,
    pub category: &'static str,
    pub expectation: String,
    pub met: bool,
    pub observed: Value,
}

#[derive(Default, Serialize)]
pub struct CleanupObservation {
    pub adapters_before_release: usize,
    pub subscriptions_before_release: usize,
    pub window_removed: bool,
    pub remaining_windows: usize,
    pub harness_weak_upgradable: bool,
    pub editor_weak_observations: Vec<WeakEditorObservation>,
    pub errors: Vec<String>,
}

#[derive(Serialize)]
pub struct Report {
    pub format_version: u32,
    pub experiment: &'static str,
    pub status: String,
    pub diagnostic_completed: bool,
    pub exit_code: i32,
    pub harness_error: Option<String>,
    pub build_mode: &'static str,
    pub target_os: &'static str,
    pub target_arch: &'static str,
    pub dependency_contract: Value,
    pub evidence_limits: Vec<&'static str>,
    pub privacy_compilation: Value,
    pub fixtures: Value,
    pub hydration_samples: Vec<Value>,
    pub phases: Vec<PhaseObservation>,
    pub expectations: Vec<Expectation>,
    pub unassigned_events_at_cleanup: Vec<EventObservation>,
    pub cleanup: CleanupObservation,
}

impl Report {
    pub fn new() -> Self {
        Self {
            format_version: 1,
            experiment: "shared-text-feasibility: NONSHARING native/API diagnostic",
            status: "running".to_owned(),
            diagnostic_completed: false,
            exit_code: 1,
            harness_error: None,
            build_mode: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            target_os: std::env::consts::OS,
            target_arch: std::env::consts::ARCH,
            dependency_contract: json!({
                "gpui-kit": "=0.9.0", "gpui-base": "0.9.0", "gpui-component": "0.9.0",
                "gpui-pre": "0.3.11", "ropey": "=2.0.0-beta.1",
                "identity_verification": "main must verify independent locked metadata before execution",
                "editor": "path ../../apps/editor; public EditorAdapter, unmodified",
                "nsh": "not used; separate JS host/panel evaluation, not this editing engine"
            }),
            evidence_limits: vec![
                "Two EditorStates are independent documents: NONSHARING, never shared editable multiview.",
                "Actual native window and Editor render/layout/paint; explicit draw return is not physical presentation/pixel evidence.",
                "Synthetic EntityInputHandler callbacks and dispatched Undo/Redo actions are not physical HID, Cocoa IME, or candidate-geometry validation.",
                "All mutable editing/view state stays on GPUI foreground. Fixture identity work and report file I/O run outside the UI thread.",
                "Pending-native predicate is public Rope instance inequality, not execution of Save, Close, or Quit product commands.",
                "Recovery payloads are actual build_recovery_commit results; no RecoveryHandle, durable writes, queue acknowledgements, or worker savepoint verification are simulated.",
                "Revision 1 and durable revision 1 are fixture hydration parameters, not observed storage acknowledgements. Dirty may remain conservative without the savepoint worker.",
                "Native undo depth/transaction count and native layout allocation sizes have no public observation here. Adapter has_undo_history is not native history depth.",
                "Every emitted edit and its immutable post-snapshot is logged. Native state at callback time may already be the final replay state.",
                "Text logging is bounded to 512 UTF-8 bytes per payload. Timed mutation includes event-observer overhead, including Rope byte equality; no full large-buffer log materialization or hashing occurs in those timings.",
                "Hydration and instrumented mutation/frame samples are single-run NONSHARING baselines, not shared performance or physical input-to-screen latency.",
                "Source predictions and product acceptance are separate. Failed acceptance is retained; all reachable cases still run. Exit 2 means completed diagnostics with failed assertions.",
            ],
            privacy_compilation: json!({
                "scenario": "private_boundary_compilation",
                "execution": "separate required-features target; not invoked by native executable",
                "command": "cargo check --locked --offline --manifest-path spikes/shared-text-feasibility/Cargo.toml --features private-probe --bin private_state_probe",
                "prerequisite": "positive native target compiles first",
                "expected_failure_codes_only": ["E0616"],
                "expected_fields": ["text", "undo_manager"]
            }),
            fixtures: Value::Null,
            hydration_samples: Vec::new(),
            phases: Vec::new(),
            expectations: Vec::new(),
            unassigned_events_at_cleanup: Vec::new(),
            cleanup: CleanupObservation::default(),
        }
    }

    pub fn expect(
        &mut self,
        row: &PhaseObservation,
        category: &'static str,
        expectation: &str,
        met: bool,
        observed: Value,
    ) {
        self.expectations.push(Expectation {
            scenario: row.scenario.clone(),
            phase: row.phase.clone(),
            category,
            expectation: expectation.to_owned(),
            met,
            observed,
        });
    }

    pub fn finish(&mut self) {
        let cleanup_failed = !self.cleanup.errors.is_empty()
            || !self.cleanup.window_removed
            || self.cleanup.remaining_windows != 0
            || self.cleanup.harness_weak_upgradable
            || self
                .cleanup
                .editor_weak_observations
                .iter()
                .any(|editor| editor.weak_upgradable);
        let assertions_failed = self
            .expectations
            .iter()
            .any(|expectation| expectation.category != "source_prediction" && !expectation.met);
        self.exit_code =
            if self.harness_error.is_some() || cleanup_failed || !self.diagnostic_completed {
                1
            } else if assertions_failed {
                2
            } else {
                0
            };
        self.status = match self.exit_code {
            0 => "completed",
            2 => "completed_with_failed_assertions",
            _ => "harness_or_cleanup_failed",
        }
        .to_owned();
    }
}
