mod harness;
mod report;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui_kit::component::input::{Redo, Undo};
use gpui_kit::{
    AnyWindowHandle, AppContext as _, AsyncApp, Bounds, Context, Entity, EntityInputHandler as _,
    QuitMode, SharedString, TitlebarOptions, Window, WindowBounds, WindowOptions, point, px, size,
};
use serde_json::json;

use harness::{Fixture, Harness};
use report::{FrameObservation, PhaseObservation, Report};

const UNICODE_TEXT: &str = "A中🙂e\u{301}\r\nB\t終\n";

struct Fixtures {
    small: Fixture,
    one_mib: Fixture,
    single: Fixture,
    unicode: Fixture,
}

impl Fixtures {
    fn create() -> Self {
        let small = UNICODE_TEXT.repeat(4096_usize.div_ceil(UNICODE_TEXT.len()));
        let row = "fn item(v: u64) -> u64 { v + 1 } // 普通 NONSHARING editor baseline\n";
        let mut large = row.repeat(1_048_576 / row.len());
        large.push_str(&"x".repeat(1_048_576 - large.len()));
        Self {
            small: Fixture::new("approximately_4KiB_Unicode_CRLF", &small),
            one_mib: Fixture::new("exactly_1MiB_normal_lines", &large),
            single: Fixture::new("single_ascii_a", "a"),
            unicode: Fixture::new("Unicode_CRLF_alias_control", UNICODE_TEXT),
        }
    }
}

struct Runner {
    window: AnyWindowHandle,
    harness: Entity<Harness>,
}

impl Runner {
    fn draw(&self, cx: &mut AsyncApp) -> Result<FrameObservation, String> {
        let start = Instant::now();
        let mut frame = self
            .window
            .update(cx, |_, window, cx| {
                let before = self.harness.read(cx).render_count;
                window.refresh();
                window.draw(cx).clear(cx);
                let after = self.harness.read(cx).render_count;
                let sequence = self.harness.update(cx, |harness, _| {
                    harness.draw_count += 1;
                    harness.draw_count
                });
                let viewport = window.viewport_size();
                FrameObservation {
                    explicit_draw_sequence: sequence,
                    draw_returned: true,
                    render_count_before: before,
                    render_count_after: after,
                    draw_and_flush_ms: 0.0,
                    viewport_pixels: [f32::from(viewport.width), f32::from(viewport.height)],
                    scale_factor: window.scale_factor(),
                    os_window_active: window.is_window_active(),
                }
            })
            .map_err(|error| format!("native draw: {error}"))?;
        frame.draw_and_flush_ms = start.elapsed().as_secs_f64() * 1000.0;
        Ok(frame)
    }

    fn phase(
        &self,
        name: &str,
        delivery: &str,
        input: bool,
        cx: &mut AsyncApp,
        action: impl FnOnce(&mut Harness, &mut Window, &mut Context<Harness>),
    ) -> Result<PhaseObservation, String> {
        let (scenario, pre_input_draw_sequence) = cx.update(|cx| {
            self.harness.update(cx, |harness, cx| {
                harness.phase = name.to_owned();
                cx.notify();
                (
                    harness.scenario.clone(),
                    input.then_some(harness.draw_count),
                )
            })
        });
        let start = Instant::now();
        self.window
            .update(cx, |_, window, cx| {
                self.harness
                    .update(cx, |harness, cx| action(harness, window, cx));
            })
            .map_err(|error| format!("{scenario}/{name} mutation: {error}"))?;
        // Every window update is top-level, not nested in another App update.
        // Its effects flush before return; this additional empty turn is explicit.
        cx.update(|_| {});
        let mutation_ms = input.then(|| start.elapsed().as_secs_f64() * 1000.0);
        let after_event_flush_before_draw = self
            .window
            .update(cx, |_, window, cx| {
                self.harness
                    .update(cx, |harness, cx| harness.observe_controls(window, cx))
            })
            .map_err(|error| format!("{scenario}/{name} pre-draw observation: {error}"))?;
        let frame = self.draw(cx)?;
        let (after_draw, events, ownership, retained, alias, pair_bytes, pair_instance) = self
            .window
            .update(cx, |_, window, cx| {
                self.harness.update(cx, |harness, cx| {
                    let controls = harness.observe_controls(window, cx);
                    let pair = harness.pair_equality(cx);
                    (
                        controls,
                        std::mem::take(&mut harness.events),
                        harness.ownership(),
                        harness.snapshot_observation(cx),
                        harness.alias_entity_id.clone(),
                        pair.0,
                        pair.1,
                    )
                })
            })
            .map_err(|error| format!("{scenario}/{name} post-draw observation: {error}"))?;
        Ok(PhaseObservation {
            scenario,
            phase: name.to_owned(),
            delivery: delivery.to_owned(),
            pre_input_draw_sequence,
            mutation_and_instrumented_event_flush_ms: mutation_ms,
            after_event_flush_before_draw,
            frame,
            after_draw,
            events,
            ownership_after_draw: ownership,
            retained_snapshot: retained,
            alias_entity_id: alias,
            pair_native_byte_equal: pair_bytes,
            pair_native_rope_instance_equal: pair_instance,
        })
    }

    fn input(
        &self,
        name: &str,
        index: usize,
        delivery: &str,
        report: &mut Report,
        cx: &mut AsyncApp,
        action: impl FnOnce(&mut Harness, &mut Window, &mut Context<Harness>),
    ) -> Result<PhaseObservation, String> {
        let preparation = self.phase(
            &format!("{name}_focus_and_draw"),
            "focus, event flush, then actual native draw",
            false,
            cx,
            |harness, window, cx| harness.control(index).adapter.focus(window, cx),
        )?;
        let label = if index == 0 { "A" } else { "B" };
        let control = preparation.control(label);
        report.expect(
            &preparation,
            "diagnostic_boundary",
            "Input target is focused and has a drawn native editor layout before delivery",
            control.focused
                && control.visible_rows.is_some()
                && preparation.frame.render_count_after > preparation.frame.render_count_before,
            json!({"focused": control.focused, "visible_rows": control.visible_rows,
                "draw": preparation.frame}),
        );
        report.phases.push(preparation);
        self.phase(name, delivery, true, cx, action)
    }

    fn install(
        &self,
        scenario: &str,
        fixture: &Fixture,
        count: usize,
        report: &mut Report,
        cx: &mut AsyncApp,
    ) -> Result<(), String> {
        let samples = self
            .window
            .update(cx, |_, window, cx| {
                self.harness.update(cx, |harness, cx| {
                    harness.install(scenario, fixture, count, window, cx)
                })
            })
            .map_err(|error| format!("install {scenario}: {error}"))?;
        report.hydration_samples.extend(samples);
        let initial = self.phase(
            "hydrated",
            "fresh real EditorAdapter instances; no input",
            false,
            cx,
            |_, _, _| {},
        )?;
        for control in &initial.after_draw {
            report.expect(&initial, "diagnostic_boundary", "Fresh adapter native and committed Rope identity matches at revision 1 with no recovery payload",
                control.native_committed_rope_instance_equal && control.visible_revision == 1 && !control.dirty
                    && control.recovery_payload.is_none(), json!(control));
        }
        report.phases.push(initial);
        Ok(())
    }
}

fn independent_control(
    runner: &Runner,
    fixture: &Fixture,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let scenario = format!("independent_states_{}", fixture.name);
    runner.install(&scenario, fixture, 2, report, cx)?;
    let configured = runner.phase(
        "independent_selection_wrap_scroll",
        "public per-state selection/scroll APIs; different adapter wrap preferences",
        false,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                state.set_selected_range(0..0, cx);
                state.set_scroll_offset(point(px(0.0), px(0.0)), cx);
                state.focus(window, cx);
            });
            harness.control(1).adapter.editor().update(cx, |state, cx| {
                state.set_selected_range_with_direction(1..4, true, cx);
                state.set_scroll_offset(point(px(0.0), px(-600.0)), cx);
            });
        },
    )?;
    report.expect(&configured, "diagnostic_boundary", "Equal initial bytes belong to distinct native entities and nonshared mutable Rope instances",
        configured.control("A").entity_id != configured.control("B").entity_id
            && configured.pair_native_byte_equal == Some(true) && configured.pair_native_rope_instance_equal == Some(false),
        json!({"A": configured.control("A").entity_id, "B": configured.control("B").entity_id,
            "byte_equal": configured.pair_native_byte_equal, "rope_instance_equal": configured.pair_native_rope_instance_equal}));
    let b_selection = configured.control("B").selection_bytes.clone();
    let b_scroll = configured.control("B").scroll_pixels;
    report.expect(
        &configured,
        "diagnostic_boundary",
        "Independent controls retain different selection, wrap and laid-out scroll state",
        configured.control("A").selection_bytes != b_selection
            && configured.control("B").selection_reversed
            && configured.control("A").applied_soft_wrap
            && !configured.control("B").applied_soft_wrap
            && configured.control("A").scroll_pixels != b_scroll,
        json!({"A": configured.control("A"), "B": configured.control("B")}),
    );
    report.phases.push(configured);
    let edited = runner.input(
        "insert_A",
        0,
        "direct EntityInputHandler::replace_text_in_range on A",
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                state.replace_text_in_range(None, "X", window, cx)
            });
        },
    )?;
    report.expect(&edited, "diagnostic_boundary", "Editing A does not mutate B's text/history/selection/wrap/scroll: NONSHARING control",
        edited.control("A").visible_revision == 2 && edited.control("B").visible_revision == 1
            && !edited.control("B").dirty && edited.edit_events("B") == 0
            && edited.pair_native_byte_equal == Some(false) && edited.control("B").native.byte_len == fixture.rope.len()
            && edited.control("B").selection_bytes == b_selection && edited.control("B").scroll_pixels == b_scroll
            && !edited.control("B").applied_soft_wrap,
        json!({"A": edited.control("A"), "B": edited.control("B"), "B_edit_events": edited.edit_events("B")}));
    let a_revision = edited.control("A").visible_revision;
    let a_byte_len = edited.control("A").native.byte_len;
    report.phases.push(edited);
    let undo_b = runner.input(
        "undo_B",
        1,
        "EditorAdapter::undo; dispatch through focused native tree",
        report,
        cx,
        |harness, window, cx| harness.control(1).adapter.undo(window, cx),
    )?;
    report.expect(
        &undo_b,
        "diagnostic_boundary",
        "Undo in B cannot undo A; B has no A history",
        undo_b.control("A").visible_revision == a_revision
            && undo_b.control("A").native.byte_len == a_byte_len
            && undo_b.control("B").visible_revision == 1
            && undo_b.edit_events("A") == 0
            && undo_b.edit_events("B") == 0,
        json!({"A": undo_b.control("A"), "B": undo_b.control("B")}),
    );
    report.phases.push(undo_b);
    let close = runner.phase(
        "drop_A",
        "drop A adapter and its two subscriptions, redraw without A",
        false,
        cx,
        |harness, _, cx| {
            harness.controls[0] = None;
            cx.notify();
        },
    )?;
    report.expect(
        &close,
        "diagnostic_boundary",
        "Closing one independent control leaves one adapter and two owned subscriptions",
        close.ownership_after_draw.owned_adapters == 1
            && close.ownership_after_draw.owned_event_subscriptions == 2,
        json!(close.ownership_after_draw),
    );
    report.phases.push(close);
    report.phases.push(runner.phase(
        "drop_A_paint_only",
        "second paint-only turn permits old frame references to retire",
        false,
        cx,
        |_, _, _| {},
    )?);
    let survivor = runner.input(
        "edit_surviving_B",
        1,
        "direct callback to still-live independent B after A is dropped",
        report,
        cx,
        |harness, window, cx| {
            harness.control(1).adapter.editor().update(cx, |state, cx| {
                state.replace_text_in_range(None, "Y", window, cx)
            });
        },
    )?;
    report.expect(
        &survivor,
        "diagnostic_boundary",
        "B remains editable after A closes (independent documents only)",
        survivor.control("B").visible_revision == 2 && survivor.edit_events("B") == 1,
        json!(survivor.control("B")),
    );
    report.phases.push(survivor);
    Ok(())
}

fn alias_control(
    runner: &Runner,
    fixture: &Fixture,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    runner.install(
        "entity_alias_and_immutable_snapshot",
        fixture,
        1,
        report,
        cx,
    )?;
    let alias = runner.input("select_through_alias", 0, "clone entity handle, retain Rope snapshot, mutate selection through alias; alias is never drawn", report, cx,
        |harness, _, cx| {
            let alias = harness.control(0).adapter.editor().clone();
            harness.alias_entity_id = Some(format!("{:?}", alias.entity_id()));
            harness.retained_snapshot = Some(alias.read(cx).text().clone());
            alias.update(cx, |state, cx| state.set_selected_range_with_direction(1..4, true, cx));
        })?;
    report.expect(&alias, "diagnostic_boundary", "Entity alias has the same identity and selection; it is not an independent view",
        alias.alias_entity_id.as_deref() == Some(alias.control("A").entity_id.as_str())
            && alias.control("A").selection_bytes == (1..4) && alias.control("A").selection_reversed
            && alias.ownership_after_draw.owned_adapters == 1,
        json!({"alias": alias.alias_entity_id, "A": alias.control("A"), "ownership": alias.ownership_after_draw}));
    report.phases.push(alias);
    let edit = runner.input(
        "replace_selected_Unicode",
        0,
        "direct native callback replaces selected 中 with X",
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                state.replace_text_in_range(None, "X", window, cx)
            });
        },
    )?;
    report.expect(
        &edit,
        "diagnostic_boundary",
        "Retained Rope is an immutable old snapshot, not live shared mutable text",
        edit.retained_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.retained.is(UNICODE_TEXT)
                && !snapshot.native_byte_equal
                && !snapshot.native_rope_instance_equal
        }) && edit.control("A").native.is("AX🙂e\u{301}\r\nB\t終\n"),
        json!({"snapshot": edit.retained_snapshot, "A": edit.control("A")}),
    );
    report.phases.push(edit);
    Ok(())
}

fn reset_control(
    runner: &Runner,
    fixture: &Fixture,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    runner.install("set_value_history_reset", fixture, 1, report, cx)?;
    let edit = runner.input(
        "insert_b",
        0,
        "direct native callback",
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                state.replace_text_in_range(None, "b", window, cx)
            });
        },
    )?;
    report.expect(
        &edit,
        "diagnostic_boundary",
        "Reset control first establishes an actual undoable native edit",
        edit.control("A").native.is("ab")
            && edit.edit_events("A") == 1
            && edit.control("A").visible_revision == 2,
        json!(edit.control("A")),
    );
    report.phases.push(edit);
    let reset = runner.input(
        "set_value_reset",
        0,
        "public EditorState::set_value, intentionally not an adapter edit API",
        report,
        cx,
        |harness, window, cx| {
            harness
                .control(0)
                .adapter
                .editor()
                .update(cx, |state, cx| state.set_value("reset", window, cx));
        },
    )?;
    report.expect(&reset, "diagnostic_boundary", "set_value suppresses edit publication and replaces native text; it is not a synchronization seam",
        reset.control("A").native.is("reset") && reset.edit_events("A") == 0
            && reset.control("A").committed.is("ab") && reset.control("A").visible_revision == 2,
        json!(reset.control("A")));
    report.phases.push(reset);
    let undo = runner.input(
        "undo_after_set_value",
        0,
        "EditorAdapter::undo routed after focus and draw",
        report,
        cx,
        |harness, window, cx| harness.control(0).adapter.undo(window, cx),
    )?;
    report.expect(
        &undo,
        "diagnostic_boundary",
        "Undo cannot restore pre-set_value history",
        undo.control("A").native.is("reset") && undo.edit_events("A") == 0,
        json!({"A": undo.control("A"), "edit_event_count": undo.edit_events("A")}),
    );
    report.phases.push(undo);
    Ok(())
}

fn mark(
    runner: &Runner,
    text: &'static str,
    name: &str,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<PhaseObservation, String> {
    runner.input(
        name,
        0,
        "direct EntityInputHandler::replace_and_mark_text_in_range(None, text, caret)",
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                let end = text.encode_utf16().count();
                state.replace_and_mark_text_in_range(None, text, Some(end..end), window, cx);
            });
        },
    )
}

fn expect_preedit(row: &PhaseObservation, expected_native: &str, report: &mut Report) {
    let control = row.control("A");
    report.expect(
        row,
        "product_acceptance",
        "Preedit changes native working text only; committed/recovery stay isolated at revision 1",
        control.native.is(expected_native)
            && control.committed.is("a")
            && control.visible_revision == 1
            && !control.dirty
            && control.active_ime
            && control.marked_utf16.is_some()
            && control.pending_native_identity_predicate
            && row.edit_events("A") == 0
            && control.recovery_payload.is_none(),
        json!({"A": control, "edit_event_count": row.edit_events("A")}),
    );
}

fn explicit_commit(
    runner: &Runner,
    fixture: &Fixture,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    runner.install(
        "marked_n_ni_Chinese_commit_undo_redo",
        fixture,
        1,
        report,
        cx,
    )?;
    for (text, name, native) in [("n", "mark_n", "an"), ("ni", "mark_ni", "ani")] {
        let row = mark(runner, text, name, report, cx)?;
        expect_preedit(&row, native, report);
        report.phases.push(row);
    }
    let commit = runner.input(
        "commit_Chinese",
        0,
        "direct EntityInputHandler::replace_text_in_range(None, 你)",
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                state.replace_text_in_range(None, "你", window, cx)
            });
        },
    )?;
    let control = commit.control("A");
    report.expect(
        &commit,
        "product_acceptance",
        "Explicit commit publishes exactly one committed mutation with no pending identity barrier",
        control.native.is("a你")
            && control.committed.is("a你")
            && control.visible_revision == 2
            && control.dirty
            && !control.active_ime
            && !control.pending_native_identity_predicate
            && commit.edit_events("A") == 1
            && control
                .recovery_payload
                .as_ref()
                .is_some_and(|payload| payload.new_revision == 2 && payload.rope.is("a你")),
        json!({"A": control, "edit_event_count": commit.edit_events("A")}),
    );
    let before_undo = control.visible_revision;
    report.phases.push(commit);
    let undo = runner.input(
        "undo_IME_transaction",
        0,
        "EditorAdapter::undo; every emitted immutable step and recovery payload retained",
        report,
        cx,
        |harness, window, cx| harness.control(0).adapter.undo(window, cx),
    )?;
    report.expect(&undo, "product_acceptance", "Undo publishes the transaction atomically, without an/ani entering edit, committed, or recovery snapshots",
        undo.control("A").native.is("a") && undo.control("A").committed.is("a")
            && !undo.control("A").pending_native_identity_predicate && undo.edit_events("A") == 1
            && undo.control("A").visible_revision == before_undo + 1 && !undo.publishes_ime_intermediates(),
        json!({"A": undo.control("A"), "edit_event_count": undo.edit_events("A"),
            "publishes_IME_intermediate_snapshots": undo.publishes_ime_intermediates(), "events": undo.events}));
    let before_redo = undo.control("A").visible_revision;
    report.phases.push(undo);
    let redo = runner.input(
        "redo_IME_transaction",
        0,
        "EditorAdapter::redo; every emitted immutable step and recovery payload retained",
        report,
        cx,
        |harness, window, cx| harness.control(0).adapter.redo(window, cx),
    )?;
    report.expect(&redo, "product_acceptance", "Redo publishes the transaction atomically, without an/ani entering edit, committed, or recovery snapshots",
        redo.control("A").native.is("a你") && redo.control("A").committed.is("a你")
            && !redo.control("A").pending_native_identity_predicate && redo.edit_events("A") == 1
            && redo.control("A").visible_revision == before_redo + 1 && !redo.publishes_ime_intermediates(),
        json!({"A": redo.control("A"), "edit_event_count": redo.edit_events("A"),
            "publishes_IME_intermediate_snapshots": redo.publishes_ime_intermediates(), "events": redo.events}));
    report.phases.push(redo);
    Ok(())
}

fn unresolved_mark(
    runner: &Runner,
    fixture: &Fixture,
    empty_cancel: bool,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let scenario = if empty_cancel {
        "empty_mark_cancel_no_following_edit"
    } else {
        "unmark_only_no_following_edit"
    };
    runner.install(scenario, fixture, 1, report, cx)?;
    let marked = mark(runner, "n", "mark_n", report, cx)?;
    expect_preedit(&marked, "an", report);
    report.phases.push(marked);
    let finished = runner.input(
        if empty_cancel {
            "empty_mark_cancel"
        } else {
            "unmark_only"
        },
        0,
        if empty_cancel {
            "direct replace_and_mark_text_in_range(None, empty, None); no settling edit follows"
        } else {
            "direct unmark_text; retains native text rather than assuming cancellation"
        },
        report,
        cx,
        |harness, window, cx| {
            harness.control(0).adapter.editor().update(cx, |state, cx| {
                if empty_cancel {
                    state.replace_and_mark_text_in_range(None, "", None, window, cx);
                } else {
                    state.unmark_text(window, cx);
                }
            });
        },
    )?;
    assess_mark_end(&finished, empty_cancel, report);
    report.phases.push(finished);
    let paint_only = runner.phase(
        "paint_only_no_subsequent_edit",
        "independent top-level draw and event flush only; no text, selection or focus mutation",
        false,
        cx,
        |_, _, _| {},
    )?;
    assess_mark_end(&paint_only, empty_cancel, report);
    report.phases.push(paint_only);
    Ok(())
}

fn assess_mark_end(row: &PhaseObservation, empty_cancel: bool, report: &mut Report) {
    let control = row.control("A");
    if empty_cancel {
        report.expect(row, "product_acceptance", "Empty-mark cancellation restores unchanged bytes, no committed revision/dirty/recovery, and a cleared identity barrier",
            control.native.is("a") && control.committed.is("a") && control.native_committed_byte_equal
                && control.native_committed_rope_instance_equal && !control.active_ime && control.visible_revision == 1
                && !control.dirty && row.edit_events("A") == 0 && control.recovery_payload.is_none(),
            json!({"A": control, "edit_event_count": row.edit_events("A"), "native_undo_depth": "not publicly observable; no Undo used to settle state"}));
    } else {
        report.expect(
            row,
            "product_acceptance",
            "Unmark-only resolves native/committed text and pending identity before any later edit",
            !control.active_ime
                && control.marked_utf16.is_none()
                && control.native_committed_byte_equal
                && !control.pending_native_identity_predicate,
            json!({"A": control, "edit_event_count": row.edit_events("A")}),
        );
        report.expect(row, "source_prediction", "Pinned-source prediction only: native an, committed a, no event, unmarked but identity barrier remains",
            control.native.is("an") && control.committed.is("a") && control.marked_utf16.is_none()
                && row.edit_events("A") == 0 && control.visible_revision == 1 && control.pending_native_identity_predicate,
            json!(control));
    }
}

fn focus_transition(
    runner: &Runner,
    fixture: &Fixture,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    runner.install("focus_switch_during_mark", fixture, 2, report, cx)?;
    let marked = mark(runner, "n", "mark_A_n", report, cx)?;
    expect_preedit(&marked, "an", report);
    report.phases.push(marked);
    let switched = runner.phase(
        "focus_B_without_edit",
        "focus independent B; flush/observe A before any callback or action",
        false,
        cx,
        |harness, window, cx| harness.control(1).adapter.focus(window, cx),
    )?;
    report.expect(
        &switched,
        "diagnostic_boundary",
        "Focus actually switches from A to B without an edit to B",
        !switched.control("A").focused
            && switched.control("B").focused
            && switched.control("B").visible_revision == 1,
        json!({"A": switched.control("A"), "B": switched.control("B")}),
    );
    report.expect(
        &switched,
        "source_prediction",
        "Pinned-source prediction only: blur is not an A composition-finalization protocol",
        switched.control("A").native.is("an")
            && switched.control("A").committed.is("a")
            && switched.control("A").pending_native_identity_predicate,
        json!(switched.control("A")),
    );
    report.phases.push(switched);
    let paint_only = runner.phase(
        "focused_B_paint_only",
        "paint-only turn before B input; no following edit to A",
        false,
        cx,
        |_, _, _| {},
    )?;
    let a_native_before = paint_only.control("A").native.text.clone();
    let a_revision_before = paint_only.control("A").visible_revision;
    report.phases.push(paint_only);
    let inserted = runner.input("synthetic_callback_explicitly_to_B", 1,
        "direct EntityInputHandler callback explicitly targets B, NOT a claim about OS text-input routing", report, cx,
        |harness, window, cx| {
            harness.control(1).adapter.editor().update(cx, |state, cx| state.replace_text_in_range(None, "z", window, cx));
        })?;
    report.expect(&inserted, "diagnostic_boundary", "Explicit B callback edits B only; separate documents do not implement a shared-document composition lease",
        inserted.control("B").native.is("az") && inserted.edit_events("B") == 1
            && inserted.control("A").native.text == a_native_before && inserted.control("A").visible_revision == a_revision_before
            && inserted.edit_events("A") == 0,
        json!({"A": inserted.control("A"), "B": inserted.control("B")}));
    report.phases.push(inserted);
    let undone = runner.input("dispatch_Undo_at_current_focus", 1,
        "Window::dispatch_action(Undo), without EditorAdapter refocusing the target inside the action", report, cx,
        |_, window, cx| window.dispatch_action(Box::new(Undo), cx))?;
    report.expect(&undone, "diagnostic_boundary", "Focused native dispatch tree routes Undo to B, leaving A's observed working bytes/revision unchanged",
        undone.control("B").native.is("a") && undone.edit_events("B") == 1 && undone.edit_events("A") == 0
            && undone.control("A").native.text == a_native_before && undone.control("A").visible_revision == a_revision_before,
        json!({"A": undone.control("A"), "B": undone.control("B")}));
    report.phases.push(undone);
    let redone = runner.input(
        "dispatch_Redo_at_current_focus",
        1,
        "Window::dispatch_action(Redo) at drawn B focus",
        report,
        cx,
        |_, window, cx| window.dispatch_action(Box::new(Redo), cx),
    )?;
    report.expect(
        &redone,
        "diagnostic_boundary",
        "Focused Redo returns B's edit without editing A",
        redone.control("B").native.is("az")
            && redone.edit_events("B") == 1
            && redone.edit_events("A") == 0
            && redone.control("A").native.text == a_native_before
            && redone.control("A").visible_revision == a_revision_before,
        json!({"A": redone.control("A"), "B": redone.control("B")}),
    );
    report.phases.push(redone);
    Ok(())
}

fn run_cases(
    runner: &Runner,
    fixtures: &Fixtures,
    report: &mut Report,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    report.fixtures = json!([
        fixtures.small.description(),
        fixtures.one_mib.description(),
        fixtures.single.description(),
        fixtures.unicode.description()
    ]);
    independent_control(runner, &fixtures.small, report, cx)?;
    independent_control(runner, &fixtures.one_mib, report, cx)?;
    alias_control(runner, &fixtures.unicode, report, cx)?;
    reset_control(runner, &fixtures.single, report, cx)?;
    explicit_commit(runner, &fixtures.single, report, cx)?;
    unresolved_mark(runner, &fixtures.single, false, report, cx)?;
    unresolved_mark(runner, &fixtures.single, true, report, cx)?;
    focus_transition(runner, &fixtures.single, report, cx)?;
    Ok(())
}

fn cleanup(runner: Runner, report: &mut Report, cx: &mut AsyncApp) {
    let harness_weak = runner.harness.downgrade();
    let watches = cx.update(|cx| {
        runner.harness.update(cx, |harness, cx| {
            let ownership = harness.ownership();
            report.cleanup.adapters_before_release = ownership.owned_adapters;
            report.cleanup.subscriptions_before_release = ownership.owned_event_subscriptions;
            report.unassigned_events_at_cleanup = std::mem::take(&mut harness.events);
            harness.release(cx);
            std::mem::take(&mut harness.watches)
        })
    });
    for _ in 0..2 {
        if let Err(error) = runner.draw(cx) {
            report.cleanup.errors.push(error);
        }
    }
    match runner
        .window
        .update(cx, |_, window, _| window.remove_window())
    {
        Ok(()) => report.cleanup.window_removed = true,
        Err(error) => report
            .cleanup
            .errors
            .push(format!("remove owned native window: {error}")),
    }
    drop(runner);
    cx.update(|_| {});
    report.cleanup.remaining_windows = cx.update(|cx| cx.windows().len());
    report.cleanup.harness_weak_upgradable = harness_weak.upgrade().is_some();
    report.cleanup.editor_weak_observations =
        watches.iter().map(harness::EditorWatch::observe).collect();
}

fn emit_report(path: &Path, report: &Report) -> Result<(), String> {
    let bytes =
        serde_json::to_vec_pretty(report).map_err(|error| format!("serialize report: {error}"))?;
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(path)?;
        file.write_all(&bytes)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        eprintln!(
            "REPORT_WRITE_FAILED: {error}\n{}",
            String::from_utf8_lossy(&bytes)
        );
        let _ = std::io::stderr().flush();
        return Err(format!("write report {}: {error}", path.display()));
    }
    println!(
        "SHARED_TEXT_DIAGNOSTIC status={} exit={} phases={} failed_assertions={} report={}",
        report.status,
        report.exit_code,
        report.phases.len(),
        report
            .expectations
            .iter()
            .filter(|expectation| expectation.category != "source_prediction" && !expectation.met)
            .count(),
        path.display()
    );
    std::io::stdout()
        .flush()
        .map_err(|error| format!("flush report announcement: {error}"))
}

async fn finish(mut report: Report, path: PathBuf, cx: &mut AsyncApp) {
    report.finish();
    let code = report.exit_code;
    let write_result = cx
        .background_spawn(async move { emit_report(&path, &report) })
        .await;
    if let Err(error) = write_result {
        eprintln!("{error}");
        std::process::exit(1);
    }
    // macOS may terminate immediately inside quit(), so cleanup and report fsync
    // must already be complete. Nonzero diagnostic outcomes preserve their status.
    if code != 0 {
        std::process::exit(code);
    }
    cx.update(|cx| cx.quit());
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let mut report_path = std::env::temp_dir().join("shared-text-feasibility.json");
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!(
                "shared-text-feasibility [--report PATH]\nNative GUI session required. Default report: {}\nExit 0: all assertions met; 2: diagnostics complete with failed assertions; 1: harness/report/cleanup error.\nPrivacy target is separate: --features private-probe --bin private_state_probe (expected E0616).",
                report_path.display()
            );
            return;
        } else if arg == "--report" {
            report_path = args.next().map(PathBuf::from).unwrap_or_else(|| {
                eprintln!("--report requires a path");
                std::process::exit(1);
            });
        } else {
            eprintln!("unknown argument: {}", arg.to_string_lossy());
            std::process::exit(1);
        }
    }
    // Fixture generation and full content identity hashing precede native startup
    // and all measurements; no GPUI entities are sent to this thread.
    let fixtures = std::thread::spawn(Fixtures::create)
        .join()
        .unwrap_or_else(|_| {
            eprintln!("fixture generation panicked before creating any native resources");
            std::process::exit(1);
        });
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            let bounds = Bounds::centered(None, size(px(1180.0), px(760.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(
                        "Shared text feasibility — NONSHARING diagnostic",
                    )),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                ..Default::default()
            };
            let opened = gpui_kit::open_window(options, cx, |window, cx| {
                // The runner, not an accidental user close, owns deterministic cleanup.
                window.on_window_should_close(cx, |_, _| false);
                cx.new(|_| Harness::new())
            });
            cx.activate(true);
            cx.spawn(async move |cx: &mut AsyncApp| {
                let mut report = Report::new();
                match opened {
                    Ok((window, harness)) => {
                        let runner = Runner { window, harness };
                        // Allow native activation/frame scheduling, outside every sample.
                        cx.background_executor()
                            .timer(Duration::from_millis(80))
                            .await;
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_cases(&runner, &fixtures, &mut report, cx)
                        }));
                        match result {
                            Ok(Ok(())) => report.diagnostic_completed = true,
                            Ok(Err(error)) => report.harness_error = Some(error),
                            Err(payload) => {
                                let message = payload
                                    .downcast_ref::<String>()
                                    .map(String::as_str)
                                    .or_else(|| payload.downcast_ref::<&str>().copied())
                                    .unwrap_or("non-string panic payload");
                                report.harness_error =
                                    Some(format!("native diagnostic panicked: {message}"));
                            }
                        }
                        cleanup(runner, &mut report, cx);
                    }
                    Err(error) => {
                        report.harness_error = Some(format!("open native window: {error}"));
                        report.cleanup.remaining_windows = cx.update(|cx| cx.windows().len());
                        report.cleanup.window_removed = report.cleanup.remaining_windows == 0;
                    }
                }
                finish(report, report_path, cx).await;
            })
            .detach();
        });
}
