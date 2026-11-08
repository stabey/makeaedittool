use super::*;
use editor_services::ContentIdentity;

pub(super) async fn run_split_scenario(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    run_startup_scenario(window, workspace, report, cx).await?;
    let large_fixture = cx.update(|cx| workspace.read(cx).session_layout().view_count() >= 100);
    if large_fixture {
        verify_fixture_visibility(workspace, report, cx)?;
        if !report.split_focus_first_budget_verified {
            measure_fixture_input(window, workspace, report, cx).await?;
        }
    } else {
        let (recovery, initial) = window.update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                if ws.session_layout().panes().count() != 1 || ws.session_layout().view_count() != 1
                    || ws.is_lifecycle_busy() || ws.search.visible || ws.palette_visible || ws.goto_visible
                    || !matches!(ws.active_slot(), Some(TabSlot::Editable(ed)) if ed.visible_revision == Revision(0)
                        && ed.committed_byte_len() == 0 && !ed.is_dirty() && !ed.has_checkpoint
                        && !ed.has_undo_history() && !ed.has_pending_native_edit(cx) && ed.meta.path.is_none())
                {
                    return Err("splits native fixture requires one untouched scratch, not an existing user session".into());
                }
                ws.set_focus_only(false, window, cx);
                ws.capture_session_view_states(cx);
                ws.persist_session_layout(cx);
                Ok((ws.recovery.clone().ok_or("missing recovery worker")?, ws.session_layout().clone()))
            })
        }).map_err(|e| e.to_string())??;
        groups::wait_for_published_layout(&recovery, &initial, cx).await?;
        crate::group_ui::verify_pane_ui_native(window, workspace, cx).await?;
        cx.update(|cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            if ws.session_layout().panes().count() < 2
                || ws.is_focus_only()
                || ws.is_compact_layout()
                || ws.is_lifecycle_busy()
                || ws.search.visible
                || ws.palette_visible
                || ws.measured_visible_pane_count() != ws.session_layout().panes().count()
            {
                return Err(
                    "native pane helper did not leave its promised settled visible tree".into(),
                );
            }
            ws.session_layout().validate().map_err(|e| e.to_string())
        })?;
        verify_protected_pane_close(window, workspace, cx).await?;
        report.split_native_ui_verified = true;
    }
    let (recovery, expected) = window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                ws.capture_session_view_states(cx);
                ws.persist_session_layout(cx);
                report.expected_group_drafts = ws
                    .all_view_ids()
                    .filter_map(|id| {
                        let target = ws.view_target(id)?;
                        let TabSlot::Editable(ed) = ws.view_slot(id)? else {
                            return None;
                        };
                        (ed.has_checkpoint || ed.visible_revision.0 > 0).then(|| {
                            (
                                target.document,
                                ContentIdentity::of_rope(ed.committed_rope()),
                                ed.visible_revision,
                            )
                        })
                    })
                    .collect();
                let expected = ws.session_layout().clone();
                report.expected_session_layout = Some(expected.clone());
                capture_workspace_metrics(ws, report);
                Ok((
                    ws.recovery.clone().ok_or("missing recovery worker")?,
                    expected,
                ))
            })
        })
        .map_err(|e| e.to_string())??;
    groups::wait_for_published_layout(&recovery, &expected, cx).await?;
    Ok(())
}

async fn verify_protected_pane_close(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                ws.all_view_ids().all(|id| {
                    matches!(ws.view_slot(id), Some(TabSlot::Editable(ed))
                        if ed.queued_revision == ed.visible_revision
                            && ed.durable_revision == ed.visible_revision
                            && !ed.has_pending_native_edit(cx))
                })
            })
        },
        "protected pane close fixture durability",
    )
    .await?;
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                let protected = |target: &ViewTarget| {
                    matches!(ws.view_slot(target.view_id), Some(TabSlot::Editable(ed)) if ed.is_dirty())
                };
                let target = ws
                    .pane_active_target(ws.active_pane_id())
                    .filter(&protected)
                    .or_else(|| {
                        ws.all_view_ids()
                            .filter_map(|id| ws.view_target(id))
                            .find(protected)
                    })
                    .ok_or("pane close cancellation needs an existing protected Editable")?;
                ws.activate_view(target, window, cx);
                let Some(TabSlot::Editable(ed)) = ws.active_slot() else {
                    return Err("protected pane activation did not retain its Editable".into());
                };
                ed.focus(window, cx);
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;
    pump_frame(window, cx)?;
    let (targets, layout, before) = window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                ws.capture_session_view_states(cx);
                let targets = ws
                    .pane_view_ids(ws.active_pane_id())
                    .ok_or("protected close pane disappeared")?
                    .iter()
                    .map(|id| {
                        ws.view_target(*id)
                            .ok_or("protected close target disappeared")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if targets.len() == ws.session_layout().view_count() {
                    return Err(
                        "pane close cancellation needs an untouched other-pane document".into(),
                    );
                }
                let before = ws
                    .all_view_ids()
                    .map(|id| -> Result<_, String> {
                        let target = ws.view_target(id).ok_or("pane audit view disappeared")?;
                        let audit = groups::snapshot(ws, target)?;
                        let Some(TabSlot::Editable(ed)) = ws.view_slot(id) else {
                            return Err("pane audit lost an Editable".into());
                        };
                        Ok((
                            target,
                            audit,
                            ed.meta.clone(),
                            ed.editor().read(cx).presentation().is_readonly(),
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((targets, ws.session_layout().clone(), before))
            })
        })
        .map_err(|e| e.to_string())??;
    window
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(crate::actions::CloseActivePaneTabs), cx);
        })
        .map_err(|e| e.to_string())?;
    wait_until(
        Duration::from_secs(5),
        cx,
        |cx| cx.update(|cx| workspace.read(cx).pending_lifecycle_sequence().is_some()),
        "rendered protected Close Active Pane Tabs confirmation",
    )
    .await?;
    // Rendering must preserve the existing whole-session readonly barrier, even
    // though only the active pane's views participate in the close transaction.
    pump_frame(window, cx)?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.session_layout() != &layout {
            return Err("protected pane close changed layout before confirmation".into());
        }
        for (target, expected, meta, _) in &before {
            let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                return Err("protected pane close replaced an editor before confirmation".into());
            };
            if !ed.editor().read(cx).presentation().is_readonly()
                || ed.has_pending_native_edit(cx)
                || &ed.meta != meta
                || groups::snapshot(ws, *target)? != *expected
            {
                return Err("rendered pane close barrier changed a buffer or lost readonly".into());
            }
        }
        Ok(())
    })?;
    groups::dispatch_dialog(window, cx, false)?;
    wait_until(
        Duration::from_secs(5),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).is_lifecycle_busy()),
        "protected pane close dialog cancellation",
    )
    .await?;
    pump_frame(window, cx)?;
    window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.capture_session_view_states(cx);
                if ws.session_layout() != &layout {
                    return Err("Cancel changed the complete pane layout or view state".into());
                }
                for (target, expected, meta, readonly) in &before {
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                        return Err("Cancel lost a target or other-pane editor".into());
                    };
                    if ed.editor().read(cx).presentation().is_readonly() != *readonly
                        || ed.has_pending_native_edit(cx)
                        || &ed.meta != meta
                        || groups::snapshot(ws, *target)? != *expected
                    {
                        return Err(
                            "Cancel failed to restore all pane buffers and readonly states".into(),
                        );
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    // Keep on the same rendered action proves the exact close targets through
    // consumer-visible removal, rather than inspecting private lifecycle state.
    let mut closed_layout = layout.clone();
    closed_layout
        .remove_views(&targets)
        .map_err(|e| e.to_string())?;
    window
        .update(cx, |_, window, cx| {
            window.dispatch_action(Box::new(crate::actions::CloseActivePaneTabs), cx);
        })
        .map_err(|e| e.to_string())?;
    wait_until(
        Duration::from_secs(5),
        cx,
        |cx| cx.update(|cx| workspace.read(cx).pending_lifecycle_sequence().is_some()),
        "second protected Close Active Pane Tabs confirmation",
    )
    .await?;
    groups::dispatch_dialog(window, cx, true)?;
    wait_until(
        Duration::from_secs(10),
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).is_lifecycle_busy()),
        "protected pane Keep close acknowledgement",
    )
    .await?;
    pump_frame(window, cx)?;
    window
        .update(cx, |_, _, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.capture_session_view_states(cx);
                if ws.session_layout() != &closed_layout {
                    return Err(
                        "Keep closed the wrong views, removed the pane, or altered other panes"
                            .into(),
                    );
                }
                for target in &targets {
                    if ws.documents.contains_key(&target.document.doc_id) {
                        return Err("acknowledged pane close retained a target editor".into());
                    }
                }
                for (target, expected, meta, readonly) in &before {
                    if targets.contains(target) {
                        continue;
                    }
                    let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else {
                        return Err("pane Keep close lost an other-pane editor".into());
                    };
                    if ed.editor().read(cx).presentation().is_readonly() != *readonly
                        || ed.has_pending_native_edit(cx)
                        || &ed.meta != meta
                        || groups::snapshot(ws, *target)? != *expected
                    {
                        return Err(
                            "pane Keep close changed another pane's buffer or readonly state"
                                .into(),
                        );
                    }
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())?
}

fn verify_fixture_visibility(
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    cx.update(|cx| {
        let ws = workspace.read(cx);
        if ws.layout_dirty || ws.layout_write_count() != 0 {
            return Err("ordinary restored fixture issued a layout write before explicit smoke persistence".into());
        }
        let count = ws.session_layout().panes().count();
        if ![1, 2, 4, 8].contains(&count) || ws.is_compact_layout() || ws.is_focus_only()
            || ws.measured_visible_pane_count() != count || ws.visible_targets().count() != count
        {
            return Err("measurement requires all logical fixture panes actually visible in the ample window".into());
        }
        if ws.budget_config.max_hydrated_editors == 1 && count > 1 {
            if !matches!(ws.active_slot(), Some(TabSlot::Editable(_))) || ws.hydrated_editor_count() != 1
                || ws.visible_targets().filter(|target| Some(target.view_id) != ws.active_view_id())
                    .any(|target| !matches!(ws.view_slot(target.view_id), Some(TabSlot::OverBudgetDraft { .. })))
            {
                return Err("focus-first one-editor admission did not retain other visible drafts as OverBudget".into());
            }
            report.split_focus_first_budget_verified = true;
        } else if ws.hydrated_editor_count() != count || ws.visible_targets()
            .any(|target| !matches!(ws.view_slot(target.view_id), Some(TabSlot::Editable(_))))
        {
            return Err("fixture did not hydrate exactly one editable document per visible pane".into());
        }
        if ws.all_view_ids().filter(|id| !ws.visible_targets().any(|target| target.view_id == *id))
            .any(|id| !matches!(ws.view_slot(id), Some(TabSlot::Dehydrated(_))))
        {
            return Err("fixture eagerly loaded a hidden metadata tab".into());
        }
        Ok(())
    })
}

async fn measure_fixture_input(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    report: &mut SmokeReport,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let (target, editor, original, revision, tree, cursor) = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                let target = ws
                    .pane_active_target(ws.active_pane_id())
                    .ok_or("fixture selection missing")?;
                let Some(TabSlot::Editable(ed)) = ws.active_slot() else {
                    return Err("fixture focus is not Editable".into());
                };
                let editor = ed.editor().clone();
                let original = ed.committed_rope().clone();
                let cursor = editor.read(cx).cursor();
                let mid = original.floor_char_boundary(original.len() / 2);
                editor.update(cx, |state, cx| {
                    state.set_selected_range(mid..mid, cx);
                    state.focus(window, cx);
                });
                Ok((
                    target,
                    editor,
                    original,
                    ed.visible_revision,
                    ws.session_layout().tree.clone(),
                    cursor,
                ))
            })
        })
        .map_err(|e| e.to_string())??;
    pump_frame(window, cx)?;
    let mut samples = Vec::with_capacity(TYPING_BENCH_OPS);
    for index in 0..TYPING_BENCH_OPS {
        let started = Instant::now();
        window
            .update(cx, |_, window, cx| -> Result<(), String> {
                if index % 2 == 0 {
                    editor.update(cx, |state, cx| {
                        state.replace_text_in_range(
                            None,
                            if index % 4 == 0 { "a" } else { "中🙂" },
                            window,
                            cx,
                        );
                    });
                } else {
                    workspace.update(cx, |ws, cx| -> Result<(), String> {
                        if ws.pane_active_target(ws.active_pane_id()) != Some(target) {
                            return Err("input measurement lost focused document identity".into());
                        }
                        ws.active_editable_mut()
                            .ok_or("input measurement lost editor")?
                            .undo(window, cx);
                        Ok(())
                    })?;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())??;
        samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let Some(TabSlot::Editable(ed)) = ws.view_slot(target.view_id) else { return Err("measurement lost editor".into()) };
        if ed.editor().entity_id() != editor.entity_id() || ed.committed_rope() != &original
            || ed.visible_revision.0 != revision.0 + TYPING_BENCH_OPS as u64
            || ws.session_layout().tree != tree || ed.has_pending_native_edit(cx)
        {
            return Err("fixture input/Undo did not preserve original body/entity/pane tree or commit every sample".into());
        }
        Ok(())
    })?;
    window
        .update(cx, |_, _, cx| {
            editor.update(cx, |state, cx| state.set_selected_range(cursor..cursor, cx));
        })
        .map_err(|e| e.to_string())?;
    samples.sort_by(f64::total_cmp);
    report.typing_doc_bytes = original.len() as u64;
    report.typing_sample_count = samples.len();
    report.typing_submit_p50_ms = Some(percentile_sorted(&samples, 0.50));
    report.typing_submit_p95_ms = Some(percentile_sorted(&samples, 0.95));
    report.typing_submit_max_ms = samples.last().copied();
    report.split_fixture_input_verified = true;
    Ok(())
}
