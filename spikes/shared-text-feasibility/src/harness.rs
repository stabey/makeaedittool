use std::time::Instant;

use editor::editor_adapter::{EditorAdapter, HydrateEditorParams};
use editor::shell::EditorViewPrefs;
use editor_services::{
    ContentIdentity, DocMeta, DocumentId, EncodingSpec, LifecycleGen, LineEnding, Revision,
    ViewState,
};
use gpui_kit::component::input::{Editor, EditorState, InputEditsEvent, InputEvent};
use gpui_kit::{
    App, Context, EntityInputHandler as _, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, Subscription, WeakEntity, Window, div, px,
};
use ropey::{Rope, extra::esoterica::ropes_are_instances};
use serde_json::{Value, json};

use crate::report::{
    ControlObservation, EditObservation, EventObservation, OwnershipObservation,
    RecoveryObservation, SnapshotObservation, TextObservation, WeakEditorObservation,
};

pub struct Fixture {
    pub name: &'static str,
    pub rope: Rope,
    pub identity: ContentIdentity,
    pub lf_count: usize,
}

impl Fixture {
    pub fn new(name: &'static str, text: &str) -> Self {
        let rope = Rope::from(text);
        let identity = ContentIdentity::of_rope(&rope);
        Self {
            name,
            rope,
            identity,
            lf_count: text.bytes().filter(|byte| *byte == b'\n').count(),
        }
    }

    pub fn description(&self) -> Value {
        json!({"name": self.name, "bytes": self.rope.len(), "lf_count": self.lf_count,
            "language": "plaintext", "classification": "NONSHARING"})
    }
}

pub struct Control {
    pub label: &'static str,
    pub adapter: EditorAdapter,
    subscriptions: Vec<Subscription>,
    event_count: u64,
}

impl Control {
    pub fn observe(&self, window: &mut Window, cx: &mut App) -> ControlObservation {
        let adapter = &self.adapter;
        let committed = adapter.committed_rope();
        let mut observation = adapter.editor().update(cx, |state, cx| {
            let marked_utf16 = state.marked_text_range(window, cx);
            let selected_utf16 = state
                .selected_text_range(false, window, cx)
                .map(|selection| selection.range);
            let instance_equal = ropes_are_instances(state.text(), committed);
            let scroll = state.scroll_offset();
            ControlObservation {
                control: self.label,
                entity_id: format!("{:?}", cx.entity_id()),
                document_id: format!("{:032x}", adapter.doc_id.0),
                native: TextObservation::rope(state.text()),
                committed: TextObservation::rope(committed),
                marked_utf16,
                selected_utf16,
                selection_bytes: state.selected_range(),
                selection_reversed: state.is_selection_reversed(),
                cursor_byte: state.cursor(),
                focused: state.focus_handle(cx).is_focused(window),
                applied_soft_wrap: adapter.applied_prefs().soft_wrap,
                scroll_pixels: [f32::from(scroll.x), f32::from(scroll.y)],
                visible_rows: state.visible_row_range(),
                line_height_pixels: state.line_height().map(f32::from),
                visible_revision: adapter.visible_revision.0,
                queued_revision: adapter.queued_revision.0,
                durable_revision: adapter.durable_revision.0,
                dirty: adapter.is_dirty(),
                adapter_has_undo_history: adapter.has_undo_history(),
                active_ime: false,
                native_committed_byte_equal: instance_equal || state.text() == committed,
                native_committed_rope_instance_equal: instance_equal,
                pending_native_identity_predicate: !instance_equal,
                recovery_payload: adapter
                    .build_recovery_commit()
                    .map(RecoveryObservation::capture),
            }
        });
        observation.active_ime = adapter.has_active_ime(window, cx);
        observation
    }
}

pub struct EditorWatch {
    pub scenario: String,
    pub control: &'static str,
    pub entity_id: String,
    pub weak: WeakEntity<EditorState>,
}

impl EditorWatch {
    pub fn observe(&self) -> WeakEditorObservation {
        WeakEditorObservation {
            scenario: self.scenario.clone(),
            control: self.control,
            entity_id: self.entity_id.clone(),
            weak_upgradable: self.weak.upgrade().is_some(),
        }
    }
}

pub struct Harness {
    pub controls: [Option<Control>; 2],
    pub watches: Vec<EditorWatch>,
    pub render_count: u64,
    pub draw_count: u64,
    pub scenario: String,
    pub phase: String,
    pub events: Vec<EventObservation>,
    pub retained_snapshot: Option<Rope>,
    pub alias_entity_id: Option<String>,
    event_sequence: u64,
}

impl Harness {
    pub fn new() -> Self {
        Self {
            controls: [None, None],
            watches: Vec::new(),
            render_count: 0,
            draw_count: 0,
            scenario: String::new(),
            phase: String::new(),
            events: Vec::new(),
            retained_snapshot: None,
            alias_entity_id: None,
            event_sequence: 0,
        }
    }

    pub fn control(&self, index: usize) -> &Control {
        self.controls[index]
            .as_ref()
            .expect("scenario control must exist")
    }

    pub fn install(
        &mut self,
        scenario: &str,
        fixture: &Fixture,
        count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Value> {
        self.controls = [None, None];
        self.retained_snapshot = None;
        self.alias_entity_id = None;
        self.scenario = scenario.to_owned();
        self.phase = "hydrate".to_owned();
        let mut samples = Vec::with_capacity(count);
        for index in 0..count {
            let label = if index == 0 { "A" } else { "B" };
            let params = HydrateEditorParams {
                doc_id: DocumentId::new(),
                lifecycle_gen: LifecycleGen(1),
                meta: DocMeta {
                    title: format!("{scenario} {label}"),
                    saved_identity: Some(fixture.identity),
                    saved_encoding: Some(EncodingSpec::Utf8),
                    saved_line_ending: Some(LineEnding::Lf),
                    ..Default::default()
                },
                rope: fixture.rope.clone(),
                initial_rev: Revision(1),
                durable_rev: Revision(1),
                is_clean_at_load: true,
                disk_conflict: false,
                initial_view: ViewState {
                    cursor_byte: fixture.rope.len() as u64,
                    scroll_byte: 0,
                },
                initial_prefs: EditorViewPrefs {
                    soft_wrap: index == 0,
                    ..Default::default()
                },
                activation_tick: 1,
            };
            let started = Instant::now();
            let adapter = EditorAdapter::new(params, window, cx);
            let hydration_ms = started.elapsed().as_secs_f64() * 1000.0;
            let entity = adapter.editor();
            self.watches.push(EditorWatch {
                scenario: scenario.to_owned(),
                control: label,
                entity_id: format!("{:?}", entity.entity_id()),
                weak: entity.downgrade(),
            });
            let edits = cx.subscribe_in(entity, window,
                move |this, entity, event: &InputEditsEvent, window, cx| {
                    let Some(control) = this.controls[index].as_mut() else { return; };
                    // A retired entity must never feed a newly installed adapter.
                    if control.adapter.editor().entity_id() != entity.entity_id() { return; }
                    let revision_before_ingest = control.adapter.visible_revision.0;
                    let steps = event.edits().iter().map(|(edit, post)| EditObservation {
                        start_byte: edit.start_byte, old_end_byte: edit.old_end_byte,
                        new_end_byte: edit.new_end_byte, post_snapshot: TextObservation::rope(post),
                    }).collect();
                    let outcome = control.adapter.ingest_edits_event(event);
                    let pending_savepoint_check = outcome.as_ref().and_then(|outcome| outcome.savepoint_check)
                        .map(|check| json!({"sequence": check.seq, "checked_revision": check.checked_rev.0,
                            "checked_text_sequence": check.checked_text_seq, "worker_executed": false}));
                    control.event_count += 1;
                    let control_event_count = control.event_count;
                    let at_callback_after_ingest = control.observe(window, cx);
                    this.event_sequence += 1;
                    this.events.push(EventObservation::Edits {
                        sequence: this.event_sequence, control: label,
                        entity_id: format!("{:?}", entity.entity_id()), control_event_count,
                        edits: steps, revision_before_ingest, adapter_ingested: outcome.is_some(),
                        pending_savepoint_check, at_callback_after_ingest,
                    });
                });
            let input = cx.subscribe(entity, move |this, entity, event: &InputEvent, _cx| {
                let Some(control) = this.controls[index].as_ref() else {
                    return;
                };
                if control.adapter.editor().entity_id() != entity.entity_id() {
                    return;
                }
                let event = match event {
                    InputEvent::Change => "change".to_owned(),
                    InputEvent::Focus => "focus".to_owned(),
                    InputEvent::Blur => "blur".to_owned(),
                    InputEvent::PressEnter { secondary, shift } => {
                        format!("press_enter secondary={secondary} shift={shift}")
                    }
                };
                this.event_sequence += 1;
                this.events.push(EventObservation::Input {
                    sequence: this.event_sequence,
                    control: label,
                    entity_id: format!("{:?}", entity.entity_id()),
                    event,
                });
            });
            self.controls[index] = Some(Control {
                label,
                adapter,
                subscriptions: vec![edits, input],
                event_count: 0,
            });
            samples.push(json!({"scenario": scenario, "control": label, "fixture_bytes": fixture.rope.len(),
                "adapter_hydration_ms": hydration_ms,
                "boundary": "EditorAdapter::new only; includes its actual String hydration; excludes subscription setup and first frame",
                "classification": "NONSHARING"}));
        }
        cx.notify();
        samples
    }

    pub fn observe_controls(&self, window: &mut Window, cx: &mut App) -> Vec<ControlObservation> {
        self.controls
            .iter()
            .flatten()
            .map(|control| control.observe(window, cx))
            .collect()
    }

    pub fn ownership(&self) -> OwnershipObservation {
        OwnershipObservation {
            owned_adapters: self.controls.iter().flatten().count(),
            owned_event_subscriptions: self
                .controls
                .iter()
                .flatten()
                .map(|control| control.subscriptions.len())
                .sum(),
            retained_immutable_snapshots: usize::from(self.retained_snapshot.is_some()),
            editors: self.watches.iter().map(EditorWatch::observe).collect(),
        }
    }

    pub fn snapshot_observation(&self, cx: &App) -> Option<SnapshotObservation> {
        let retained = self.retained_snapshot.as_ref()?;
        let native = self.controls[0].as_ref()?.adapter.editor().read(cx).text();
        Some(SnapshotObservation {
            retained: TextObservation::rope(retained),
            native_byte_equal: retained == native,
            native_rope_instance_equal: ropes_are_instances(retained, native),
        })
    }

    pub fn pair_equality(&self, cx: &App) -> (Option<bool>, Option<bool>) {
        let (Some(a), Some(b)) = (&self.controls[0], &self.controls[1]) else {
            return (None, None);
        };
        let a = a.adapter.editor().read(cx).text();
        let b = b.adapter.editor().read(cx).text();
        (Some(a == b), Some(ropes_are_instances(a, b)))
    }

    pub fn release(&mut self, cx: &mut Context<Self>) {
        self.controls = [None, None];
        self.retained_snapshot = None;
        cx.notify();
    }
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.render_count += 1;
        div()
            .size_full()
            .flex()
            .flex_col()
            .p_2()
            .gap_2()
            .child(format!(
                "NONSHARING diagnostic — {} / {}",
                self.scenario, self.phase
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .children(self.controls.iter().flatten().map(|control| {
                        div()
                            .w(px(560.0))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(format!("Independent {}", control.label))
                            .child(Editor::new(control.adapter.editor()).h(px(620.0)).w_full())
                    })),
            )
    }
}
