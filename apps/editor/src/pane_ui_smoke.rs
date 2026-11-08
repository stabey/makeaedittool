use std::time::{Duration, Instant};

use super::pane_ui::{MIN_PANE_HEIGHT_PX, MIN_PANE_WIDTH_PX, should_use_compact_layout};
use super::smoke::{
    EditableAuditSnapshot, TabStripItem, capture_editable_snapshots, dispatch_ascii_text,
    dispatch_key, dispatch_mouse, drag_nudge_point, draw_frame, focus_active_editor_or_workspace,
    scroll_and_hit_point, settle_tick,
};
use crate::actions::EqualizePanes;
use crate::app::{EditorWorkspace, TabSlot};
use editor_services::{
    DocumentKey, GroupColor, GroupCommand, GroupId, LayoutRoot, PaneCommand, PaneId, SplitAxis,
    SplitNodeId, ViewId, ViewTarget,
};
use gpui_kit::component::WindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AsyncApp, Entity, Focusable as _, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, Size, point, px, size,
};

#[cfg(target_os = "macos")]
const SPLIT_RIGHT_KEY: &str = "cmd-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_RIGHT_KEY: &str = "ctrl-\\";

#[cfg(target_os = "macos")]
const SPLIT_DOWN_KEY: &str = "cmd-shift-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_DOWN_KEY: &str = "ctrl-shift-\\";

#[cfg(target_os = "macos")]
const SPLIT_LEFT_KEY: &str = "cmd-alt-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_LEFT_KEY: &str = "ctrl-alt-\\";

#[cfg(target_os = "macos")]
const SPLIT_UP_KEY: &str = "cmd-alt-shift-\\";
#[cfg(not(target_os = "macos"))]
const SPLIT_UP_KEY: &str = "ctrl-alt-shift-\\";

#[cfg(target_os = "macos")]
const SPLIT_MOVE_UP_KEY: &str = "cmd-alt-k";
#[cfg(not(target_os = "macos"))]
const SPLIT_MOVE_UP_KEY: &str = "ctrl-alt-k";

#[cfg(target_os = "macos")]
const MOVE_NEXT_PANE_KEY: &str = "cmd-alt-shift-n";
#[cfg(not(target_os = "macos"))]
const MOVE_NEXT_PANE_KEY: &str = "ctrl-alt-shift-n";

#[cfg(target_os = "macos")]
const MOVE_PREV_PANE_KEY: &str = "cmd-alt-shift-p";
#[cfg(not(target_os = "macos"))]
const MOVE_PREV_PANE_KEY: &str = "ctrl-alt-shift-p";

#[cfg(target_os = "macos")]
const EQUALIZE_KEY: &str = "cmd-alt-=";
#[cfg(not(target_os = "macos"))]
const EQUALIZE_KEY: &str = "ctrl-alt-=";

#[cfg(target_os = "macos")]
const TOGGLE_FOCUS_ONLY_KEY: &str = "cmd-alt-z";
#[cfg(not(target_os = "macos"))]
const TOGGLE_FOCUS_ONLY_KEY: &str = "ctrl-alt-z";

#[cfg(target_os = "macos")]
const CLOSE_PANE_MERGE_KEY: &str = "cmd-alt-x";
#[cfg(not(target_os = "macos"))]
const CLOSE_PANE_MERGE_KEY: &str = "ctrl-alt-x";

#[cfg(target_os = "macos")]
const CLOSE_PANE_TABS_KEY: &str = "cmd-alt-shift-w";
#[cfg(not(target_os = "macos"))]
const CLOSE_PANE_TABS_KEY: &str = "ctrl-alt-shift-w";

#[cfg(target_os = "macos")]
const UNDO_KEY: &str = "cmd-z";
#[cfg(not(target_os = "macos"))]
const UNDO_KEY: &str = "ctrl-z";

#[cfg(target_os = "macos")]
const SEARCH_KEY: &str = "cmd-f";
#[cfg(not(target_os = "macos"))]
const SEARCH_KEY: &str = "ctrl-f";

#[cfg(target_os = "macos")]
const GOTO_KEY: &str = "cmd-l";
#[cfg(not(target_os = "macos"))]
const GOTO_KEY: &str = "ctrl-g";

#[cfg(target_os = "macos")]
const PALETTE_KEY: &str = "cmd-shift-p";
#[cfg(not(target_os = "macos"))]
const PALETTE_KEY: &str = "ctrl-shift-p";

/// Exercises real rendered GPUI layout, hitbox, mouse drag/drop/cancel, divider resize/cancel,
/// compact/focus-only transitions, and focus-isolated keyboard/overlay routing across split panes.
pub(crate) async fn verify_pane_ui_native(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let result = run_verify_panes_inner(window, workspace, cx).await;
    if result.is_err() {
        let _ = window.update(cx, |_, window, cx| {
            let _ = cx.stop_active_drag(window);
            window.clear_notifications(cx);
            workspace.update(cx, |ws, cx| {
                ws.group_ui.drag_preview = None;
                let _ = ws.cancel_divider_drag(cx);
                ws.close_overlays(window, cx);
                cx.notify();
            });
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }
    result
}

async fn wait_until(
    timeout: Duration,
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    mut pred: impl FnMut(&mut AsyncApp) -> bool,
    label: &str,
) -> Result<(), String> {
    let started = Instant::now();
    loop {
        draw_frame(window, cx)?;
        if pred(cx) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(format!("pane_ui_smoke timed out waiting for {label}"));
        }
        cx.background_executor()
            .timer(Duration::from_millis(12))
            .await;
    }
}

async fn pump_layout_frames(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    rounds: usize,
) -> Result<(), String> {
    for _ in 0..rounds {
        draw_frame(window, cx)?;
        settle_tick(window, cx).await?;
    }
    draw_frame(window, cx)
}

async fn resize_window_and_settle(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    target: Size<Pixels>,
) -> Result<(), String> {
    let prior_vp = window
        .update(cx, |_, window, _| {
            let vp = window.viewport_size();
            window.resize(target);
            vp
        })
        .map_err(|e| format!("window.resize({target:?}) failed: {e}"))?;
    let expect_width_change = (prior_vp.width.as_f32() - target.width.as_f32()).abs() > 4.0;
    wait_until(
        Duration::from_secs(3),
        window,
        cx,
        |cx| {
            window
                .update(cx, |_, window, cx| {
                    let cur_vp = window.viewport_size();
                    if expect_width_change
                        && (cur_vp.width.as_f32() - target.width.as_f32()).abs() > 32.0
                    {
                        return false;
                    }
                    let ws = workspace.read(cx);
                    if ws.group_ui.panes.compact_sync_pending.get() {
                        return false;
                    }
                    let Some((seq, root_b)) = ws.group_ui.panes.measured_root_bounds.get() else {
                        return false;
                    };
                    if seq != ws.group_ui.panes.current_frame_seq() {
                        return false;
                    }
                    let desired = should_use_compact_layout(
                        &ws.session_layout().tree.root,
                        root_b,
                        &|id, fb| ws.pane_split_ratio(id).unwrap_or(fb),
                    );
                    desired == ws.is_compact_layout()
                })
                .unwrap_or(false)
        },
        "async window resize and compact-layout reconciliation",
    )
    .await?;
    draw_frame(window, cx)
}

fn click_point(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    pt: Point<Pixels>,
) -> Result<(), String> {
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )
}

fn drag_and_drop_points(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    src: Point<Pixels>,
    nudge: Point<Pixels>,
    dst: Point<Pixels>,
) -> Result<(), String> {
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: src,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: src,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: nudge,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    if !cx.update(|cx| cx.has_active_drag()) {
        return Err(format!(
            "MouseDown+MouseMove at {src:?} did not start GPUI drag"
        ));
    }
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: dst,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )
}

fn assert_visible_panes_measured(
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    expected: usize,
) -> Result<(), String> {
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let actual_panes = ws.session_layout().panes().count();
        let measured = ws.measured_visible_pane_count();
        if actual_panes != expected
            || ws.is_compact_layout()
            || ws.is_focus_only()
            || measured != expected
        {
            return Err(format!(
                "expected {expected} visible measured panes, got panes={actual_panes}, measured={measured}, compact={}, focus_only={}, root_bounds={:?}",
                ws.is_compact_layout(),
                ws.is_focus_only(),
                ws.group_ui.panes.root_bounds()
            ));
        }
        for pane in ws.session_layout().panes() {
            let b = ws
                .observed_pane_leaf_bounds(pane.id)
                .ok_or_else(|| format!("missing observed leaf bounds for {:?}", pane.id))?;
            if b.size.width.as_f32() + 0.05 < MIN_PANE_WIDTH_PX
                || b.size.height.as_f32() + 0.05 < MIN_PANE_HEIGHT_PX
            {
                return Err(format!(
                    "pane {:?} measured below 240x160 minimum: {:?}",
                    pane.id, b
                ));
            }
        }
        Ok(())
    })
}

fn single_doc_snapshot(
    ws: &EditorWorkspace,
    document: DocumentKey,
) -> Result<EditableAuditSnapshot, String> {
    let doc_id = document.doc_id;
    capture_editable_snapshots(ws)
        .into_iter()
        .find(|s| s.doc_id == doc_id)
        .ok_or_else(|| format!("missing EditableAuditSnapshot for {doc_id:?}"))
}

async fn wait_editable_durable(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    view_id: ViewId,
) -> Result<(), String> {
    wait_until(
        Duration::from_secs(10),
        window,
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                matches!(
                    ws.view_slot(view_id),
                    Some(TabSlot::Editable(ed))
                        if !ed.has_pending_native_edit(cx)
                            && ed.visible_revision.0 > 0
                            && ed.durable_revision == ed.visible_revision
                )
            })
        },
        "durable recovery commit for editable split tab",
    )
    .await
}

async fn phase_1_visibility_compact_and_directions(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<(PaneId, PaneId, ViewTarget, EditableAuditSnapshot), String> {
    let ample_size = size(px(1440.0), px(860.0));
    resize_window_and_settle(window, workspace, cx, ample_size).await?;
    focus_active_editor_or_workspace(window, workspace, cx)?;

    let root_b = cx
        .update(|cx| workspace.read(cx).group_ui.panes.root_bounds())
        .ok_or("missing measured root_bounds after initial ample resize")?;
    if root_b.size.width < px(1000.0) || root_b.size.height < px(400.0) {
        return Err(format!(
            "host display clamped window below 8-pane 4x2 minimum (root_bounds={root_b:?})"
        ));
    }

    let v_a = cx.update(|cx| -> Result<ViewTarget, String> {
        let ws = workspace.read(cx);
        ws.pane_active_target(ws.active_pane_id())
            .ok_or_else(|| "missing initial scratch target".into())
    })?;
    dispatch_ascii_text(window, cx, "alpha-seed")?;
    wait_editable_durable(window, workspace, cx, v_a.view_id).await?;
    let snap_a_initial = cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))?;
    if !snap_a_initial.dirty || !snap_a_initial.has_undo {
        return Err("initial scratch edit did not establish dirty + undo state".into());
    }

    // 1a. Actual 2 visible panes
    dispatch_key(window, cx, SPLIT_RIGHT_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 2)?;

    // 1b. Actual 4 visible panes (2x2)
    let two_panes: Vec<PaneId> = cx.update(|cx| {
        workspace
            .read(cx)
            .session_layout()
            .panes()
            .map(|p| p.id)
            .collect()
    });
    for pid in two_panes {
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| ws.activate_pane(pid, window, cx))
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        dispatch_key(window, cx, SPLIT_DOWN_KEY)?;
    }
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 4)?;

    // 1c. Actual 8 visible panes (4x2) + 9th split refusal
    let four_panes: Vec<PaneId> = cx.update(|cx| {
        workspace
            .read(cx)
            .session_layout()
            .panes()
            .map(|p| p.id)
            .collect()
    });
    for pid in four_panes {
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| ws.activate_pane(pid, window, cx))
            })
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        dispatch_key(window, cx, SPLIT_RIGHT_KEY)?;
    }
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 8)?;

    let eight_tree = cx.update(|cx| workspace.read(cx).session_layout().tree.clone());
    dispatch_key(window, cx, SPLIT_RIGHT_KEY)?;
    if cx.update(|cx| workspace.read(cx).session_layout().panes().count() != 8) {
        return Err("9th split was not refused at MAX_PANES=8".into());
    }
    window
        .update(cx, |_, window, cx| window.clear_notifications(cx))
        .map_err(|e| e.to_string())?;
    draw_frame(window, cx)?;

    // 1d. Window shrink -> automatic compact mode preserving 8-pane tree, then expand back -> 8 visible
    resize_window_and_settle(window, workspace, cx, size(px(760.0), px(540.0))).await?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if !ws.is_compact_layout() || ws.measured_visible_pane_count() != 1 {
            return Err(format!(
                "expected compact single-pane presentation at 760x540: compact={}, measured={}",
                ws.is_compact_layout(),
                ws.measured_visible_pane_count()
            ));
        }
        if ws.session_layout().tree.root != eight_tree.root {
            return Err("window shrink mutated logical 8-pane split tree".into());
        }
        Ok(())
    })?;

    resize_window_and_settle(window, workspace, cx, ample_size).await?;
    assert_visible_panes_measured(workspace, cx, 8)?;

    // 1e. Explicit Focus-Only mode + F6 / Shift-F6 keyboard cycling
    let before_cycle = cx.update(|cx| workspace.read(cx).active_pane_id());
    dispatch_key(window, cx, TOGGLE_FOCUS_ONLY_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    if cx.update(|cx| {
        let ws = workspace.read(cx);
        !ws.is_focus_only()
            || ws.measured_visible_pane_count() != 1
            || ws.session_layout().panes().count() != 8
    }) {
        return Err(
            "Focus-Only toggle did not isolate active pane while preserving 8 panes".into(),
        );
    }
    dispatch_key(window, cx, "f6")?;
    if cx.update(|cx| workspace.read(cx).active_pane_id() == before_cycle) {
        return Err("F6 did not advance active_pane_id in Focus-Only mode".into());
    }
    dispatch_key(window, cx, "shift-f6")?;
    if cx.update(|cx| workspace.read(cx).active_pane_id() != before_cycle) {
        return Err("Shift-F6 did not cycle back to prior active_pane_id".into());
    }
    dispatch_key(window, cx, TOGGLE_FOCUS_ONLY_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 8)?;

    // Merge back down to 1 pane via CloseActivePane shortcut without closing v_a
    for _ in 0..7 {
        dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;
    }
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 1)?;
    if cx.update(|cx| workspace.read(cx).session_layout().view_count() != 1) {
        return Err("CloseActivePane merge closed v_a instead of merging".into());
    }

    // 1f. Verify Left and Up directional splits (new empty pane placed in `first` child!)
    let p_base = cx.update(|cx| workspace.read(cx).active_pane_id());
    dispatch_key(window, cx, SPLIT_LEFT_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        match &ws.session_layout().tree.root {
            LayoutRoot::Split {
                axis: SplitAxis::Horizontal,
                first,
                second,
                ..
            } if matches!(first.as_ref(), LayoutRoot::Pane { pane, .. } if pane.id == ws.active_pane_id() && pane.tabs.is_empty())
                && matches!(second.as_ref(), LayoutRoot::Pane { pane, .. } if pane.id == p_base) =>
            {
                Ok(())
            }
            other => Err(format!("SplitPaneLeft produced unexpected tree: {other:?}")),
        }
    })?;
    dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;

    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, SPLIT_UP_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        match &ws.session_layout().tree.root {
            LayoutRoot::Split {
                axis: SplitAxis::Vertical,
                first,
                second,
                ..
            } if matches!(first.as_ref(), LayoutRoot::Pane { pane, .. } if pane.id == ws.active_pane_id() && pane.tabs.is_empty())
                && matches!(second.as_ref(), LayoutRoot::Pane { pane, .. } if pane.id == p_base) =>
            {
                Ok(())
            }
            other => Err(format!("SplitPaneUp produced unexpected tree: {other:?}")),
        }
    })?;
    dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;

    // Create 2 horizontal panes [p_left(v_a), p_right(empty)] and verify Empty-pane B4 focus & GoTo/Palette dismissal
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, SPLIT_RIGHT_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    let p_right = cx.update(|cx| workspace.read(cx).active_pane_id());
    window
        .update(cx, |_, window, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            if ws.pane_active_target(p_right).is_some() {
                return Err("new split right pane should be empty".into());
            }
            if window.focused(cx).as_ref() != Some(&ws.focus_handle) {
                return Err("Empty active pane did not focus EditorWorkspace.focus_handle".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;

    // Typing + Undo on Empty active pane must never reach unfocused Editable `v_a`
    dispatch_ascii_text(window, cx, "x")?;
    dispatch_key(window, cx, UNDO_KEY)?;
    let snap_after_empty_keys =
        cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))?;
    if snap_after_empty_keys != snap_a_initial {
        return Err("typing/Undo on Empty pane mutated unfocused Editable document".into());
    }

    // B4: GoTo & Command Palette open/dismissal on Empty pane keep focus on workspace, never old Editor
    dispatch_key(window, cx, GOTO_KEY)?;
    if !cx.update(|cx| workspace.read(cx).goto_visible) {
        return Err("GoTo shortcut did not open goto bar on Empty pane".into());
    }
    dispatch_key(window, cx, GOTO_KEY)?;
    let empty_goto_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            !ws.goto_visible && window.focused(cx).as_ref() == Some(&ws.focus_handle)
        })
        .map_err(|e| e.to_string())?;
    if !empty_goto_ok {
        return Err("closing GoTo on Empty pane failed to focus EditorWorkspace".into());
    }

    dispatch_key(window, cx, PALETTE_KEY)?;
    if !cx.update(|cx| workspace.read(cx).palette_visible) {
        return Err("Palette shortcut did not open Command Palette on Empty pane".into());
    }
    dispatch_key(window, cx, "escape")?;
    let empty_palette_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            !ws.palette_visible
                && ws.active_pane_id() == p_right
                && window.focused(cx).as_ref() == Some(&ws.focus_handle)
        })
        .map_err(|e| e.to_string())?;
    if !empty_palette_ok {
        return Err("dismissing Palette on Empty pane lost Empty pane or Workspace focus".into());
    }

    Ok((p_base, p_right, v_a, snap_a_initial))
}

async fn phase_2_moves_drops_and_group_persistence(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    p_left: PaneId,
    p_right: PaneId,
    v_a: ViewTarget,
) -> Result<(GroupId, GroupId, ViewTarget, ViewTarget), String> {
    let (g_alpha, g_beta, v_c, v_b) = window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<_, String> {
                ws.activate_pane(p_left, window, cx)
                    .map_err(|e| e.to_string())?;
                let g_alpha = ws
                    .apply_group_command(
                        GroupCommand::Create {
                            name: "Alpha".into(),
                            color: GroupColor::Blue,
                            before: None,
                            initial_view: Some(v_a),
                        },
                        window,
                        cx,
                    )
                    .map_err(|e| e.to_string())?
                    .created_group
                    .ok_or("missing g_alpha")?;
                let g_beta = ws
                    .apply_group_command(
                        GroupCommand::Create {
                            name: "Beta".into(),
                            color: GroupColor::Green,
                            before: None,
                            initial_view: None,
                        },
                        window,
                        cx,
                    )
                    .map_err(|e| e.to_string())?
                    .created_group
                    .ok_or("missing g_beta")?;
                let v_c = ws
                    .create_untitled_tab(true, window, cx)
                    .ok_or("failed to create v_c")?;
                ws.apply_group_command(
                    GroupCommand::MoveView {
                        target: v_c,
                        group: g_alpha,
                        before: None,
                    },
                    window,
                    cx,
                )
                .map_err(|e| e.to_string())?;
                let v_b = ws
                    .create_untitled_tab(true, window, cx)
                    .ok_or("failed to create v_b")?;
                ws.apply_group_command(
                    GroupCommand::MoveView {
                        target: v_b,
                        group: g_beta,
                        before: None,
                    },
                    window,
                    cx,
                )
                .map_err(|e| e.to_string())?;
                Ok((g_alpha, g_beta, v_c, v_b))
            })
        })
        .map_err(|e| e.to_string())??;

    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_ascii_text(window, cx, "bravo-seed")?;
    wait_editable_durable(window, workspace, cx, v_b.view_id).await?;

    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.apply_pane_command(
                    PaneCommand::Activate {
                        pane: p_left,
                        view: Some(v_c),
                    },
                    window,
                    cx,
                )
            })
        })
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_ascii_text(window, cx, "charlie-seed")?;
    wait_editable_durable(window, workspace, cx, v_c.view_id).await?;

    // Activate v_b so v_a is non-selected at index 0 of [v_a, v_c, v_b] in p_left
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.apply_pane_command(
                    PaneCommand::Activate {
                        pane: p_left,
                        view: Some(v_b),
                    },
                    window,
                    cx,
                )
            })
        })
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    pump_layout_frames(window, cx, 2).await?;

    let pre_moves_snaps = cx.update(|cx| capture_editable_snapshots(workspace.read(cx)));
    if pre_moves_snaps.len() != 3 {
        return Err(format!(
            "expected 3 editable documents before moves, got {}",
            pre_moves_snaps.len()
        ));
    }

    // 2a. B3 Own-pane center drop: activates v_a without reordering [v_a, v_c, v_b] or changing group
    let (va_pt, vp) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_left,
        TabStripItem::TabPill(v_a.view_id),
    )?;
    let p_left_center = cx
        .update(|cx| workspace.read(cx).observed_pane_body_bounds(p_left))
        .ok_or("missing p_left body bounds")?
        .center();
    drag_and_drop_points(
        window,
        cx,
        va_pt,
        drag_nudge_point(va_pt, vp),
        p_left_center,
    )?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.pane_view_ids(p_left) != Some(&[v_a.view_id, v_c.view_id, v_b.view_id][..]) {
            return Err(format!(
                "own-pane center drop reordered p_left tabs: {:?}",
                ws.pane_view_ids(p_left)
            ));
        }
        if ws.active_view_id() != Some(v_a.view_id)
            || ws.session_layout().view(v_a.view_id).map(|v| v.group_id) != Some(g_alpha)
        {
            return Err("own-pane center drop did not activate v_a or altered group_id".into());
        }
        Ok(())
    })?;

    // 2b. Cross-pane center drop: moves v_c to p_right preserving group `Alpha` and exact entity identity
    let (vc_pt, vp) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_left,
        TabStripItem::TabPill(v_c.view_id),
    )?;
    let p_right_center = cx
        .update(|cx| workspace.read(cx).observed_pane_body_bounds(p_right))
        .ok_or("missing p_right body bounds")?
        .center();
    drag_and_drop_points(
        window,
        cx,
        vc_pt,
        drag_nudge_point(vc_pt, vp),
        p_right_center,
    )?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.pane_view_ids(p_right) != Some(&[v_c.view_id][..])
            || ws.session_layout().view(v_c.view_id).map(|v| v.group_id) != Some(g_alpha)
        {
            return Err("cross-pane center drop failed to move v_c into p_right with Alpha".into());
        }
        if ws.documents.len() != 3 || capture_editable_snapshots(ws) != pre_moves_snaps {
            return Err("cross-pane center drop cloned or mutated Editable document state".into());
        }
        Ok(())
    })?;

    // 2c. Cross-pane group-header drop: moves v_c from p_right onto Beta header in p_left (regroups Alpha -> Beta)
    let (vc_right_pt, vp) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_right,
        TabStripItem::TabPill(v_c.view_id),
    )?;
    let (beta_hdr_pt, _) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_left,
        TabStripItem::GroupHeader(g_beta),
    )?;
    drag_and_drop_points(
        window,
        cx,
        vc_right_pt,
        drag_nudge_point(vc_right_pt, vp),
        beta_hdr_pt,
    )?;
    if cx.update(|cx| {
        let ws = workspace.read(cx);
        ws.session_layout().pane_for_view(v_c.view_id) != Some(p_left)
            || ws.session_layout().view(v_c.view_id).map(|v| v.group_id) != Some(g_beta)
    }) {
        return Err("cross-pane group-header drop did not move+regroup v_c into Beta".into());
    }

    // 2d. Keyboard MoveActiveTabToNextPane / PrevPane + Cross-pane TabPill drop
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.apply_pane_command(
                    PaneCommand::Activate {
                        pane: p_left,
                        view: Some(v_b),
                    },
                    window,
                    cx,
                )
            })
        })
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, MOVE_NEXT_PANE_KEY)?;
    if cx.update(|cx| workspace.read(cx).pane_view_ids(p_right) != Some(&[v_b.view_id][..])) {
        return Err("MoveActiveTabToNextPane did not move v_b into p_right".into());
    }
    dispatch_key(window, cx, MOVE_PREV_PANE_KEY)?;
    if cx.update(|cx| {
        workspace
            .read(cx)
            .session_layout()
            .pane_for_view(v_b.view_id)
            != Some(p_left)
    }) {
        return Err("MoveActiveTabToPrevPane did not return v_b to p_left".into());
    }
    dispatch_key(window, cx, MOVE_NEXT_PANE_KEY)?;

    // Cross-pane tab-pill drop: drag v_c onto v_b pill in p_right (places v_c before v_b in Beta)
    let (vc_left_pt, vp) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_left,
        TabStripItem::TabPill(v_c.view_id),
    )?;
    let (vb_right_pt, _) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_right,
        TabStripItem::TabPill(v_b.view_id),
    )?;
    drag_and_drop_points(
        window,
        cx,
        vc_left_pt,
        drag_nudge_point(vc_left_pt, vp),
        vb_right_pt,
    )?;
    if cx.update(|cx| {
        workspace.read(cx).pane_view_ids(p_right) != Some(&[v_c.view_id, v_b.view_id][..])
    }) {
        return Err("cross-pane tab-pill drop did not place v_c before v_b in p_right".into());
    }

    // 2e. Escape mid-drag cancellation over edge-split zone, then valid edge-drop split (Down)
    let layout_before_edge_cancel = cx.update(|cx| workspace.read(cx).session_layout().clone());
    let (vc_src_pt, vp) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_right,
        TabStripItem::TabPill(v_c.view_id),
    )?;
    let right_body = cx
        .update(|cx| workspace.read(cx).observed_pane_body_bounds(p_right))
        .ok_or("missing p_right body bounds for edge split")?;
    let bottom_edge_pt = point(right_body.center().x, right_body.bottom() - px(14.0));

    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: vc_src_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: vc_src_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: drag_nudge_point(vc_src_pt, vp),
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: bottom_edge_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_key(window, cx, "escape")?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: bottom_edge_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;
    if cx.update(|cx| workspace.read(cx).session_layout() != &layout_before_edge_cancel) {
        return Err("Escape mid-drag cancel over edge zone still mutated SessionLayout".into());
    }

    // Valid edge-drop split (Down) of v_c onto p_right bottom edge -> creates 3rd pane!
    let (vc_src_pt2, vp2) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        p_right,
        TabStripItem::TabPill(v_c.view_id),
    )?;
    drag_and_drop_points(
        window,
        cx,
        vc_src_pt2,
        drag_nudge_point(vc_src_pt2, vp2),
        bottom_edge_pt,
    )?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 3)?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.documents.len() != 3 || capture_editable_snapshots(ws) != pre_moves_snaps {
            return Err("edge-drop split cloned or mutated Editable document state".into());
        }
        Ok(())
    })?;

    // Also exercise keyboard SplitAndMoveActiveTabUp on p_right -> 4 panes, then merge back to 2 panes
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.activate_pane(p_right, window, cx))
        })
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, SPLIT_MOVE_UP_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 4)?;

    // Merge the two sub-panes back so we have 2 clean side-by-side panes [p_left(v_a), p_other(v_b, v_c)]
    dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;
    dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 2)?;
    if cx.update(|cx| capture_editable_snapshots(workspace.read(cx)) != pre_moves_snaps) {
        return Err(
            "CloseActivePane merge altered Editable document identity/revision/undo".into(),
        );
    }

    Ok((g_alpha, g_beta, v_b, v_c))
}

async fn phase_3a_focus_isolation_and_paged_b4(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    p_left: PaneId,
    v_a: ViewTarget,
    v_b: ViewTarget,
    v_c: ViewTarget,
) -> Result<(PaneId, SplitNodeId, std::path::PathBuf), String> {
    let p_second = cx
        .update(|cx| {
            workspace
                .read(cx)
                .session_layout()
                .panes()
                .map(|p| p.id)
                .find(|&id| id != p_left)
        })
        .ok_or("missing second pane in phase 3")?;

    // 3a. Capture-phase click in Pane 1 -> typing goes ONLY to v_a; click in Pane 2 -> typing + Undo go ONLY to Pane 2
    let left_pt = cx
        .update(|cx| workspace.read(cx).observed_pane_body_bounds(p_left))
        .ok_or("missing p_left body bounds")?
        .center();
    click_point(window, cx, left_pt)?;
    dispatch_ascii_text(window, cx, "!L")?;
    wait_editable_durable(window, workspace, cx, v_a.view_id).await?;

    let snap_b_before = cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_b.document))?;
    let snap_c_before = cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_c.document))?;
    let a_has_bang_l = cx.update(|cx| {
        matches!(
            workspace.read(cx).view_slot(v_a.view_id),
            Some(TabSlot::Editable(ed)) if ed.committed_rope().to_string().contains("!L")
        )
    });
    if !a_has_bang_l {
        return Err("capture-phase click on p_left did not route typing into v_a".into());
    }

    let right_pt = cx
        .update(|cx| workspace.read(cx).observed_pane_body_bounds(p_second))
        .ok_or("missing p_second body bounds")?
        .center();
    click_point(window, cx, right_pt)?;
    let p2_active = cx
        .update(|cx| workspace.read(cx).pane_active_target(p_second))
        .ok_or("p_second missing active target")?;
    let snap_a_locked = cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))?;

    dispatch_ascii_text(window, cx, "!R")?;
    if cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))? != snap_a_locked {
        return Err("typing in p_second mutated unfocused v_a in p_left".into());
    }
    dispatch_key(window, cx, UNDO_KEY)?;
    if cx.update(|cx| {
        matches!(
            workspace.read(cx).view_slot(p2_active.view_id),
            Some(TabSlot::Editable(ed)) if ed.committed_rope().to_string().contains("!R")
        )
    }) {
        dispatch_key(window, cx, UNDO_KEY)?;
    }
    wait_editable_durable(window, workspace, cx, p2_active.view_id).await?;
    if cx.update(|cx| {
        matches!(
            workspace.read(cx).view_slot(p2_active.view_id),
            Some(TabSlot::Editable(ed)) if ed.committed_rope().to_string().contains("!R")
        )
    }) {
        return Err("Undo in p_second did not revert '!R' in focused pane".into());
    }
    if cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))? != snap_a_locked {
        return Err("Undo in p_second mutated unfocused v_a in p_left".into());
    }
    let _ = (snap_b_before, snap_c_before);

    let scratch_dir =
        std::env::temp_dir().join(format!("editor-pane-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&scratch_dir).map_err(|e| e.to_string())?;

    // Save focus isolation (macOS atomic save supported; non-macOS save intentionally disabled per ADR-008)
    #[cfg(target_os = "macos")]
    {
        use crate::actions::SaveFile;
        let save_path = scratch_dir.join("save_focus_pane2.txt");
        std::fs::write(&save_path, b"base\n").map_err(|e| e.to_string())?;
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| {
                    let _ = ws.activate_pane(p_second, window, cx);
                    ws.open_path(save_path.clone(), window, cx);
                });
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(10),
            window,
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    matches!(
                        ws.active_slot(),
                        Some(TabSlot::Editable(ed)) if ed.meta.path.as_deref() == Some(save_path.as_path())
                    )
                })
            },
            "opening save_focus_pane2.txt",
        )
        .await?;
        focus_active_editor_or_workspace(window, workspace, cx)?;
        dispatch_ascii_text(window, cx, "saved!")?;
        let save_target = cx
            .update(|cx| workspace.read(cx).pane_active_target(p_second))
            .ok_or("missing save_target")?;
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| ws.on_save_file(&SaveFile, window, cx));
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(10),
            window,
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    matches!(ws.view_slot(save_target.view_id), Some(TabSlot::Editable(ed)) if !ed.is_dirty())
                })
            },
            "saving focused pane file",
        )
        .await?;
        if cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))? != snap_a_locked {
            return Err("SaveFile in p_second altered dirty state of unfocused v_a".into());
        }
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| ws.request_close_view(save_target, window, cx));
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(10),
            window,
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    !ws.is_lifecycle_busy()
                        && ws.session_layout().view(save_target.view_id).is_none()
                })
            },
            "closing save_focus_pane2.txt",
        )
        .await?;
    }

    let root_split = cx
        .update(|cx| match workspace.read(cx).session_layout().tree.root {
            LayoutRoot::Split { node_id, .. } => Some(node_id),
            _ => None,
        })
        .ok_or("expected root SplitNodeId for 2-pane layout")?;

    Ok((p_second, root_split, scratch_dir))
}

async fn phase_3b_paged_resize_b3_and_close_pane_tabs(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    p_left: PaneId,
    p_second: PaneId,
    root_split: SplitNodeId,
    scratch_dir: std::path::PathBuf,
    v_a: ViewTarget,
    v_b: ViewTarget,
    v_c: ViewTarget,
) -> Result<(), String> {
    // Split p_second Down -> p_throwaway (3 visible panes: 1 left, 2 right)
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.activate_pane(p_second, window, cx))
        })
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    dispatch_key(window, cx, SPLIT_DOWN_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 3)?;
    let p_throwaway = cx.update(|cx| workspace.read(cx).active_pane_id());

    // Open TWO NUL-byte Paged files in p_second and p_throwaway simultaneously (verifies zero Paged ElementId collision!)
    let mut bin_payload = vec![b'p'; 256 * 1024];
    bin_payload[64] = 0;
    bin_payload[128] = b'\n';
    let paged_path_1 = scratch_dir.join("paged_split_1.bin");
    let paged_path_2 = scratch_dir.join("paged_split_2.bin");
    std::fs::write(&paged_path_1, &bin_payload).map_err(|e| e.to_string())?;
    std::fs::write(&paged_path_2, &bin_payload).map_err(|e| e.to_string())?;

    for (pid, ppath) in [(p_second, &paged_path_1), (p_throwaway, &paged_path_2)] {
        let p_clone = ppath.clone();
        window
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| {
                    let _ = ws.activate_pane(pid, window, cx);
                    ws.open_path(p_clone, window, cx);
                });
            })
            .map_err(|e| e.to_string())?;
        wait_until(
            Duration::from_secs(10),
            window,
            cx,
            |cx| {
                cx.update(|cx| {
                    let ws = workspace.read(cx);
                    ws.pane_active_target(pid).is_some_and(|t| {
                        matches!(ws.view_slot(t.view_id), Some(TabSlot::Paged(pv)) if pv.current_slice.is_some())
                    })
                })
            },
            "opening Paged binary tab in split pane",
        )
        .await?;
    }
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 3)?;

    let paged_1_target = cx
        .update(|cx| workspace.read(cx).pane_active_target(p_second))
        .ok_or("missing paged_1_target")?;

    // B4 on Paged active pane (`p_throwaway`): GoTo & Palette open/dismissal focus Workspace, never unfocused Editor
    let snap_a_pre_paged = cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))?;
    dispatch_key(window, cx, GOTO_KEY)?;
    dispatch_key(window, cx, GOTO_KEY)?;
    let paged_goto_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            !ws.goto_visible && window.focused(cx).as_ref() == Some(&ws.focus_handle)
        })
        .map_err(|e| e.to_string())?;
    if !paged_goto_ok {
        return Err("GoTo close on Paged pane failed to focus EditorWorkspace".into());
    }
    dispatch_key(window, cx, PALETTE_KEY)?;
    dispatch_key(window, cx, "escape")?;
    let paged_palette_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            !ws.palette_visible && window.focused(cx).as_ref() == Some(&ws.focus_handle)
        })
        .map_err(|e| e.to_string())?;
    if !paged_palette_ok {
        return Err("Palette Escape on Paged pane failed to focus EditorWorkspace".into());
    }
    dispatch_ascii_text(window, cx, "q")?;
    if cx.update(|cx| single_doc_snapshot(workspace.read(cx), v_a.document))? != snap_a_pre_paged {
        return Err("typing on Paged active pane mutated unfocused Editable v_a".into());
    }

    // B3: Streaming search + active divider resize Escape cancellation & pixel-clamped preview
    dispatch_key(window, cx, SEARCH_KEY)?;
    dispatch_ascii_text(window, cx, "needle_absent")?;
    let div_center = cx
        .update(|cx| workspace.read(cx).observed_split_divider_bounds(root_split))
        .ok_or("missing root_split divider bounds")?
        .center();
    let far_left_pt = point(px(14.0), div_center.y);

    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: div_center,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: far_left_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.active_pane_resize() != Some(root_split)
            || ws.pane_split_ratio(root_split) == Some(5000)
            || ws.session_layout().split_ratio(root_split) != Some(5000)
        {
            return Err(
                "divider drag did not establish transient uncommitted preview ratio".into(),
            );
        }
        let left_b = ws
            .observed_pane_leaf_bounds(p_left)
            .ok_or("missing p_left bounds during clamped resize")?;
        if left_b.size.width.as_f32() + 0.05 < MIN_PANE_WIDTH_PX {
            return Err(format!(
                "divider clamp allowed p_left below 240px: {:?}",
                left_b
            ));
        }
        Ok(())
    })?;

    dispatch_key(window, cx, "escape")?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: far_left_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;
    if cx.update(|cx| {
        let ws = workspace.read(cx);
        ws.active_pane_resize().is_some()
            || ws.pane_split_ratio(root_split) != Some(5000)
            || ws.session_layout().split_ratio(root_split) != Some(5000)
    }) {
        return Err("Escape during search did not cancel divider resize back to 5000 bps".into());
    }

    // B3: Divider drag commit AND leaf-count-weighted Equalize preserve `search_query_input` focus!
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.search_query_input
                    .update(cx, |inp, cx| inp.focus(window, cx));
            });
        })
        .map_err(|e| e.to_string())?;
    draw_frame(window, cx)?;
    let search_fh = cx.update(|cx| {
        let ws = workspace.read(cx);
        ws.search_query_input.read(cx).focus_handle(cx)
    });
    let drag_commit_pt = point(div_center.x + px(130.0), div_center.y);
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: div_center,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: drag_commit_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: drag_commit_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;
    let commit_focus_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            ws.session_layout()
                .split_ratio(root_split)
                .is_some_and(|r| r > 5200)
                && window.focused(cx).as_ref() == Some(&search_fh)
        })
        .map_err(|e| e.to_string())?;
    if !commit_focus_ok {
        return Err("divider commit failed or stole focus from search_query_input".into());
    }

    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                ws.on_equalize_panes(&EqualizePanes, window, cx)
            });
        })
        .map_err(|e| e.to_string())?;
    draw_frame(window, cx)?;
    let eq_focus_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            // 1 leaf in `first`, 2 leaves in `second` -> leaf-count-weighted Equalize = 3333 bps!
            ws.session_layout().split_ratio(root_split) == Some(3333)
                && window.focused(cx).as_ref() == Some(&search_fh)
        })
        .map_err(|e| e.to_string())?;
    if !eq_focus_ok {
        return Err(
            "EqualizePanes did not produce leaf-count-weighted 3333 bps or stole search focus"
                .into(),
        );
    }

    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| ws.close_overlays(window, cx));
        })
        .map_err(|e| e.to_string())?;
    wait_until(
        Duration::from_secs(5),
        window,
        cx,
        |cx| cx.update(|cx| !workspace.read(cx).search.is_running()),
        "search completion after close_overlays",
    )
    .await?;

    // Explicit CloseActivePaneTabs on p_throwaway closes ONLY p_throwaway's tabs, leaving an Empty pane!
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| -> Result<(), String> {
                ws.apply_pane_command(
                    PaneCommand::MoveView {
                        target: paged_1_target,
                        source: p_second,
                        destination: p_throwaway,
                        group: GroupId::UNGROUPED,
                        before: None,
                    },
                    window,
                    cx,
                )
                .map_err(|e| e.to_string())?;
                ws.activate_pane(p_throwaway, window, cx)
                    .map_err(|e| e.to_string())
            })
        })
        .map_err(|e| e.to_string())??;
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, CLOSE_PANE_TABS_KEY)?;
    wait_until(
        Duration::from_secs(10),
        window,
        cx,
        |cx| {
            cx.update(|cx| {
                let ws = workspace.read(cx);
                !ws.is_lifecycle_busy() && ws.pane_view_ids(p_throwaway) == Some(&[][..])
            })
        },
        "CloseActivePaneTabs completion",
    )
    .await?;
    let _ = std::fs::remove_dir_all(&scratch_dir);

    if cx.update(|cx| workspace.read(cx).session_layout().view_count() != 3) {
        return Err("CloseActivePaneTabs closed tabs outside the active pane".into());
    }

    // Close empty p_throwaway -> 2 panes, equalize to 5000 via shortcut, then commit a distinct ratio for restart!
    dispatch_key(window, cx, CLOSE_PANE_MERGE_KEY)?;
    pump_layout_frames(window, cx, 2).await?;
    assert_visible_panes_measured(workspace, cx, 2)?;
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, EQUALIZE_KEY)?;
    if cx.update(|cx| workspace.read(cx).session_layout().split_ratio(root_split) != Some(5000)) {
        return Err("EQUALIZE_KEY on 2 panes did not set 5000 bps".into());
    }

    let div_pt2 = cx
        .update(|cx| workspace.read(cx).observed_split_divider_bounds(root_split))
        .ok_or("missing final divider bounds")?
        .center();
    let final_ratio_pt = point(div_pt2.x + px(95.0), div_pt2.y);
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: div_pt2,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: final_ratio_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: final_ratio_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;
    pump_layout_frames(window, cx, 2).await?;

    for vid in [v_a.view_id, v_b.view_id, v_c.view_id] {
        wait_editable_durable(window, workspace, cx, vid).await?;
    }
    assert_visible_panes_measured(workspace, cx, 2)?;
    Ok(())
}

async fn run_verify_panes_inner(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    let (p_left, p_right, v_a, _snap_a) =
        phase_1_visibility_compact_and_directions(window, workspace, cx).await?;
    let (_g_alpha, _g_beta, v_b, v_c) =
        phase_2_moves_drops_and_group_persistence(window, workspace, cx, p_left, p_right, v_a)
            .await?;
    let (p_second, root_split, scratch_dir) =
        phase_3a_focus_isolation_and_paged_b4(window, workspace, cx, p_left, v_a, v_b, v_c).await?;
    phase_3b_paged_resize_b3_and_close_pane_tabs(
        window,
        workspace,
        cx,
        p_left,
        p_second,
        root_split,
        scratch_dir,
        v_a,
        v_b,
        v_c,
    )
    .await
}
