use std::collections::HashMap;
use std::time::Duration;

use editor_services::{
    ContentIdentity, DocumentId, GroupCommand, GroupId, LifecycleGen, PaneId, Revision,
    SessionLayout, ViewId,
};
use gpui_kit::{
    AnyWindowHandle, AsyncApp, Bounds, Entity, EntityId, Focusable as _, Keystroke, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, PlatformInput, Point, Size,
    point, px,
};

use super::DragPreviewSession;
use crate::app::{EditorWorkspace, TabSlot};

/// Evidence flags returned by `verify_group_ui_native` after real `Window::dispatch_event`
/// mouse drag/drop/cancel and `Window::dispatch_keystroke` keyboard/modal assertions complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupUiNativeEvidence {
    pub(crate) drag_drop_verified: bool,
    pub(crate) drag_cancel_verified: bool,
    pub(crate) keyboard_modal_verified: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabStripItem {
    GroupHeader(GroupId),
    TabPill(ViewId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotVariantTag {
    Editable,
    Dehydrated,
    Hydrating,
    Paged,
    OverBudgetDraft,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EditableAuditSnapshot {
    pub(super) doc_id: DocumentId,
    pub(super) lifecycle_gen: LifecycleGen,
    pub(super) editor_entity: EntityId,
    pub(super) visible_rev: Revision,
    pub(super) queued_rev: Revision,
    pub(super) durable_rev: Revision,
    pub(super) text_seq: u64,
    pub(super) dirty: bool,
    pub(super) has_undo: bool,
    pub(super) identity: ContentIdentity,
}

#[cfg(target_os = "macos")]
const CREATE_EMPTY_GROUP_SHORTCUT: &str = "cmd-alt-shift-g";
#[cfg(not(target_os = "macos"))]
const CREATE_EMPTY_GROUP_SHORTCUT: &str = "ctrl-alt-shift-g";

#[cfg(target_os = "macos")]
const RENAME_GROUP_SHORTCUT: &str = "cmd-alt-r";
#[cfg(not(target_os = "macos"))]
const RENAME_GROUP_SHORTCUT: &str = "ctrl-alt-r";

#[cfg(target_os = "macos")]
const TOGGLE_COLLAPSE_SHORTCUT: &str = "cmd-alt-m";
#[cfg(not(target_os = "macos"))]
const TOGGLE_COLLAPSE_SHORTCUT: &str = "ctrl-alt-m";

#[cfg(target_os = "macos")]
const OPEN_RECOVERY_SHORTCUT: &str = "cmd-alt-b";
#[cfg(not(target_os = "macos"))]
const OPEN_RECOVERY_SHORTCUT: &str = "ctrl-alt-b";

fn slot_tag(slot: &TabSlot) -> SlotVariantTag {
    match slot {
        TabSlot::Editable(_) => SlotVariantTag::Editable,
        TabSlot::Dehydrated(_) => SlotVariantTag::Dehydrated,
        TabSlot::Hydrating { .. } => SlotVariantTag::Hydrating,
        TabSlot::Paged(_) => SlotVariantTag::Paged,
        TabSlot::OverBudgetDraft { .. } => SlotVariantTag::OverBudgetDraft,
        TabSlot::Failed { .. } => SlotVariantTag::Failed,
    }
}

fn capture_slot_tags(ws: &EditorWorkspace) -> HashMap<DocumentId, SlotVariantTag> {
    ws.documents
        .iter()
        .map(|(&doc_id, slot)| (doc_id, slot_tag(slot)))
        .collect()
}

pub(super) fn capture_editable_snapshots(ws: &EditorWorkspace) -> Vec<EditableAuditSnapshot> {
    let mut out: Vec<EditableAuditSnapshot> = ws
        .documents
        .values()
        .filter_map(|slot| match slot {
            TabSlot::Editable(ed) => Some(EditableAuditSnapshot {
                doc_id: ed.doc_id,
                lifecycle_gen: ed.lifecycle_gen,
                editor_entity: ed.editor().entity_id(),
                visible_rev: ed.visible_revision,
                queued_rev: ed.queued_revision,
                durable_rev: ed.durable_revision,
                text_seq: ed.current_text_seq(),
                dirty: ed.is_dirty(),
                has_undo: ed.has_undo_history(),
                identity: ContentIdentity::of_rope(ed.committed_rope()),
            }),
            _ => None,
        })
        .collect();
    out.sort_by_key(|s| s.doc_id.0);
    out
}

pub(super) fn draw_frame(window: &AnyWindowHandle, cx: &mut AsyncApp) -> Result<(), String> {
    window
        .update(cx, |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .map_err(|e| format!("group_ui_smoke draw_frame failed: {e}"))
}

pub(super) async fn settle_tick(window: &AnyWindowHandle, cx: &mut AsyncApp) -> Result<(), String> {
    cx.background_executor()
        .timer(Duration::from_millis(8))
        .await;
    draw_frame(window, cx)
}

pub(super) fn dispatch_key(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    spec: &'static str,
) -> Result<(), String> {
    draw_frame(window, cx)?;
    let stroke = Keystroke::parse(spec).map_err(|e| format!("invalid keystroke '{spec}': {e}"))?;
    window
        .update(cx, |_, window, cx| {
            window.dispatch_keystroke(stroke, cx);
        })
        .map_err(|e| format!("dispatch_keystroke('{spec}') failed: {e}"))?;
    draw_frame(window, cx)
}

pub(super) fn dispatch_ascii_text(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    text: &str,
) -> Result<(), String> {
    for ch in text.chars() {
        draw_frame(window, cx)?;
        let s = ch.to_string();
        let stroke = Keystroke {
            modifiers: Modifiers::default(),
            key: s.clone(),
            key_char: Some(s),
        };
        window
            .update(cx, |_, window, cx| {
                window.dispatch_keystroke(stroke, cx);
            })
            .map_err(|e| format!("dispatch_keystroke char '{ch}' failed: {e}"))?;
        draw_frame(window, cx)?;
    }
    Ok(())
}

pub(super) fn dispatch_mouse(
    window: &AnyWindowHandle,
    cx: &mut AsyncApp,
    input: PlatformInput,
) -> Result<(), String> {
    window
        .update(cx, |_, window, cx| {
            window.dispatch_event(input, cx);
        })
        .map_err(|e| format!("dispatch_event mouse failed: {e}"))?;
    draw_frame(window, cx)
}

/// Computes `(target_child_index, expected_total_children)` for a pane's `#pstr` tab strip using
/// the exact child emission order of `EditorWorkspace::render_pane_tab_strip`.
fn locate_tab_strip_child(
    ws: &EditorWorkspace,
    pane_id: PaneId,
    wanted: TabStripItem,
) -> Option<(usize, usize)> {
    let session = ws.session_layout();
    let ordered_views = ws.pane_view_ids(pane_id)?;
    let show_ungrouped_header = session.tree.groups.len() > 1
        || session
            .group(GroupId::UNGROUPED)
            .is_some_and(|g| g.collapsed);

    let mut found = None;
    let mut child_ix = 0usize;
    let mut tab_cursor = 0usize;

    for group in &session.tree.groups {
        let seg_start = tab_cursor;
        while tab_cursor < ordered_views.len()
            && session
                .view(ordered_views[tab_cursor])
                .is_some_and(|v| v.group_id == group.id)
        {
            tab_cursor += 1;
        }
        let group_tabs = &ordered_views[seg_start..tab_cursor];
        let render_header = group.id != GroupId::UNGROUPED || show_ungrouped_header;
        if render_header {
            if wanted == TabStripItem::GroupHeader(group.id) {
                found = Some(child_ix);
            }
            child_ix += 1;
        }
        if !group.collapsed {
            for &view_id in group_tabs {
                if ws.view_target(view_id).is_none() || ws.view_slot(view_id).is_none() {
                    continue;
                }
                if wanted == TabStripItem::TabPill(view_id) {
                    found = Some(child_ix);
                }
                child_ix += 1;
            }
        }
    }

    // Trailing `new_group_slot` + `trailing_strip_drop`
    let total_children = child_ix + 2;
    found.map(|ix| (ix, total_children))
}

/// Scrolls the given pane's tab strip so `item` is visible, draws a real frame, validates the
/// prepainted child count and viewport containment, and returns a safe interior hit point
/// (avoiding trailing close/menu buttons) plus current viewport size.
pub(super) fn scroll_and_hit_point(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
    pane_id: PaneId,
    item: TabStripItem,
) -> Result<(Point<Pixels>, Size<Pixels>), String> {
    draw_frame(window, cx)?;
    let (child_ix, expected_total) = cx
        .update(|cx| locate_tab_strip_child(workspace.read(cx), pane_id, item))
        .ok_or_else(|| {
            format!("tab strip item {item:?} in pane {pane_id:?} is not currently rendered")
        })?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let scroll = ws
            .group_ui
            .panes
            .strip_scroll(pane_id)
            .ok_or_else(|| format!("missing strip_scroll handle for pane {pane_id:?}"))?;
        scroll.scroll_to_item(child_ix);
        Ok(())
    })?;
    draw_frame(window, cx)?;

    window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            let scroll = ws
                .group_ui
                .panes
                .strip_scroll(pane_id)
                .ok_or_else(|| format!("missing strip_scroll handle for pane {pane_id:?}"))?;
            let actual_total = scroll.children_count();
            if actual_total != expected_total {
                return Err(format!(
                    "tab strip prepainted child count mismatch for {item:?}: actual={actual_total}, expected={expected_total}"
                ));
            }
            let raw_bounds = scroll
                .bounds_for_item(child_ix)
                .ok_or_else(|| format!("missing prepainted bounds for tab strip child {child_ix}"))?;
            let offset = scroll.offset();
            let painted = Bounds::new(raw_bounds.origin + offset, raw_bounds.size);
            if painted.size.width <= px(4.0) || painted.size.height <= px(4.0) {
                return Err(format!(
                    "degenerate painted bounds for {item:?}: {:?}",
                    painted
                ));
            }
            let inset_x = match item {
                // Left color-dot / padding region of group header (outside collapse/menu buttons)
                TabStripItem::GroupHeader(_) => px(8.0).min(painted.size.width * 0.25),
                // Left title region of tab pill (well clear of trailing close button)
                TabStripItem::TabPill(_) => px(14.0).min(painted.size.width * 0.35),
            };
            let hit = point(painted.left() + inset_x, painted.center().y);
            let strip_bounds = scroll.bounds();
            if !strip_bounds.contains(&hit) {
                return Err(format!(
                    "computed hit point {hit:?} for {item:?} lies outside tab-strip mask {strip_bounds:?}"
                ));
            }
            let viewport = window.viewport_size();
            let viewport_bounds = Bounds::new(Point::default(), viewport);
            if !viewport_bounds.contains(&hit) {
                return Err(format!(
                    "computed hit point {hit:?} for {item:?} lies outside window viewport {viewport:?}"
                ));
            }
            Ok((hit, viewport))
        })
        .map_err(|e| format!("scroll_and_hit_point window update failed: {e}"))?
}

pub(super) fn drag_nudge_point(origin: Point<Pixels>, viewport: Size<Pixels>) -> Point<Pixels> {
    let step = px(20.0);
    let nx = if origin.x + step + px(8.0) < viewport.width {
        origin.x + step
    } else {
        (origin.x - step).max(px(4.0))
    };
    point(nx, origin.y)
}

pub(super) fn focus_active_editor_or_workspace(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    window
        .update(cx, |_, window, cx| {
            workspace.update(cx, |ws, cx| {
                if let Some(TabSlot::Editable(ed)) = ws.active_slot() {
                    ed.focus(window, cx);
                } else {
                    ws.focus_handle.focus(window, cx);
                }
            });
        })
        .map_err(|e| format!("focus_active_editor_or_workspace failed: {e}"))?;
    draw_frame(window, cx)
}

/// Exercises real rendered GPUI `Window::dispatch_event` mouse drag/drop/cancel and
/// `Window::dispatch_keystroke` keyboard/modal flows on `--smoke=groups` without creating or
/// closing any `DocumentId`s, without hydrating inactive `Dehydrated` views, and restoring the
/// exact pre-call `SessionLayout` and focus before returning.
pub(crate) async fn verify_group_ui_native(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<GroupUiNativeEvidence, String> {
    let result = run_verify_inner(window, workspace, cx).await;
    if result.is_err() {
        let _ = window.update(cx, |_, window, cx| {
            let _ = cx.stop_active_drag(window);
            workspace.update(cx, |ws, cx| {
                ws.group_ui.drag_preview = None;
                let _ = ws.cancel_divider_drag(cx);
                ws.group_ui.editor_modal = None;
                if ws.group_ui.recovery_modal.take().is_some() {
                    ws.cancel_recovery_request(cx);
                }
                cx.notify();
            });
            window.refresh();
            window.draw(cx).clear(cx);
        });
    }
    result
}

async fn run_verify_inner(
    window: &AnyWindowHandle,
    workspace: &Entity<EditorWorkspace>,
    cx: &mut AsyncApp,
) -> Result<GroupUiNativeEvidence, String> {
    draw_frame(window, cx)?;

    let (
        orig_layout,
        orig_slot_tags,
        orig_hydrated_count,
        orig_editables,
        orig_pane,
        v_active,
        orig_group,
        orig_was_collapsed,
        orig_next_in_group,
        orig_group_order,
        orig_strip_offset,
    ) = cx.update(|cx| -> Result<_, String> {
        let ws = workspace.read(cx);
        if ws.is_lifecycle_busy() {
            return Err("verify_group_ui_native requires idle lifecycle state".into());
        }
        let active_pane = ws.active_pane_id();
        let ordered = ws
            .pane_view_ids(active_pane)
            .ok_or("active pane missing from session layout")?;
        if ordered.len() < 2 {
            return Err(format!(
                "verify_group_ui_native requires at least 2 open views in active pane (found {})",
                ordered.len()
            ));
        }
        let active_id = ws
            .active_view_id()
            .ok_or("verify_group_ui_native requires an active view")?;
        let active_target = ws
            .view_target(active_id)
            .ok_or("active view missing target")?;
        let layout: SessionLayout = ws.session_layout().clone();
        let active_lv = layout
            .view(active_id)
            .ok_or("active view missing in session layout")?;
        let grp_id = active_lv.group_id;
        let was_collapsed = layout.group(grp_id).is_some_and(|g| g.collapsed);

        let group_members: Vec<ViewId> = layout
            .pane(active_pane)
            .ok_or("active pane missing in layout")?
            .tabs
            .iter()
            .copied()
            .filter(|id| layout.view(*id).is_some_and(|v| v.group_id == grp_id))
            .collect();
        let pos_in_grp = group_members
            .iter()
            .position(|&id| id == active_id)
            .ok_or("active view missing from its group's member slice")?;
        let next_in_grp = group_members.get(pos_in_grp + 1).copied();
        let grp_order: Vec<GroupId> = layout.tree.groups.iter().map(|g| g.id).collect();
        let strip_off = ws
            .group_ui
            .panes
            .strip_scroll(active_pane)
            .map(|s| s.offset())
            .unwrap_or_default();

        Ok((
            layout,
            capture_slot_tags(ws),
            ws.hydrated_editor_count(),
            capture_editable_snapshots(ws),
            active_pane,
            active_target,
            grp_id,
            was_collapsed,
            next_in_grp,
            grp_order,
            strip_off,
        ))
    })?;

    focus_active_editor_or_workspace(window, workspace, cx)?;

    if orig_was_collapsed {
        dispatch_key(window, cx, TOGGLE_COLLAPSE_SHORTCUT)?;
        let expanded = cx.update(|cx| {
            workspace
                .read(cx)
                .session_layout()
                .group(orig_group)
                .is_some_and(|g| !g.collapsed)
        });
        if !expanded {
            return Err("shortcut failed to expand initially collapsed active group".into());
        }
    }

    let pre_modal_focus = window
        .update(cx, |_, window, cx| window.focused(cx))
        .map_err(|e| e.to_string())?;
    if pre_modal_focus.is_none() {
        return Err("expected non-empty focus handle before opening group modal".into());
    }

    // -------------------------------------------------------------------------
    // 1. Native Keyboard Shortcut + Modal Focus / Escape Cancel / Enter Submit
    // -------------------------------------------------------------------------
    dispatch_key(window, cx, CREATE_EMPTY_GROUP_SHORTCUT)?;
    let modal_cancel_check = window
        .update(cx, |_, window, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            let modal = ws
                .group_ui
                .editor_modal
                .as_ref()
                .ok_or("CreateEmptyGroup shortcut did not open GroupEditorModal")?;
            let input_fh = modal.name_input.read(cx).focus_handle(cx);
            if window.focused(cx).as_ref() != Some(&input_fh) {
                return Err("GroupEditorModal did not focus its name_input handle".into());
            }
            if window.focused(cx) == pre_modal_focus {
                return Err("focus remained on underlying editor while modal was open".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    modal_cancel_check?;

    dispatch_key(window, cx, "escape")?;
    let post_escape_check = window
        .update(cx, |_, window, cx| -> Result<(), String> {
            let ws = workspace.read(cx);
            if ws.group_ui.editor_modal.is_some() {
                return Err("Escape keystroke did not dismiss GroupEditorModal".into());
            }
            if ws.session_layout().tree.groups.len() != orig_group_order.len() {
                return Err("cancelled GroupEditorModal mutated group count".into());
            }
            if window.focused(cx) != pre_modal_focus {
                return Err(
                    "Escape dismissal of GroupEditorModal failed to restore prior focus".into(),
                );
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    post_escape_check?;

    // Reopen modal via shortcut, type real characters into focused Input, and press Enter.
    dispatch_key(window, cx, CREATE_EMPTY_GROUP_SHORTCUT)?;
    dispatch_ascii_text(window, cx, "ui-native-grp")?;
    let typed_val = cx.update(|cx| {
        workspace
            .read(cx)
            .group_ui
            .editor_modal
            .as_ref()
            .map(|m| m.name_input.read(cx).value().to_string())
    });
    if typed_val.as_deref() != Some("ui-native-grp") {
        return Err(format!(
            "character keystrokes did not populate modal input: got {typed_val:?}"
        ));
    }

    dispatch_key(window, cx, "enter")?;
    settle_tick(window, cx).await?;

    let g_temp = window
        .update(cx, |_, window, cx| -> Result<GroupId, String> {
            let ws = workspace.read(cx);
            if ws.group_ui.editor_modal.is_some() {
                return Err("Enter keystroke did not submit and close GroupEditorModal".into());
            }
            if window.focused(cx) != pre_modal_focus {
                return Err("GroupEditorModal Enter submit did not restore prior focus".into());
            }
            let created = ws
                .session_layout()
                .tree
                .groups
                .iter()
                .find(|g| !orig_group_order.contains(&g.id))
                .ok_or("Enter submit did not create new custom group in SessionLayout")?;
            if created.name != "ui-native-grp" {
                return Err(format!("unexpected created group name '{}'", created.name));
            }
            Ok(created.id)
        })
        .map_err(|e| e.to_string())??;

    // -------------------------------------------------------------------------
    // 2. Native Mouse Drag + Escape Mid-Drag Cancellation (`drag_cancel_verified`)
    // -------------------------------------------------------------------------
    let layout_before_cancel = cx.update(|cx| workspace.read(cx).session_layout().clone());
    let (src_tab_pt, viewport) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::TabPill(v_active.view_id),
    )?;
    let drag_start_move = drag_nudge_point(src_tab_pt, viewport);

    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: src_tab_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: src_tab_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: drag_start_move,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;

    cx.update(|cx| -> Result<(), String> {
        if !cx.has_active_drag() {
            return Err(
                "MouseDown+MouseMove on rendered tab pill did not activate GPUI drag".into(),
            );
        }
        let ws = workspace.read(cx);
        if ws.group_ui.drag_preview
            != Some(DragPreviewSession::Tab {
                source_pane: orig_pane,
                target: v_active,
            })
        {
            return Err(
                "tab drag did not record DragPreviewSession::Tab { source_pane, target }".into(),
            );
        }
        Ok(())
    })?;

    let (dst_temp_hdr_pt, _) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::GroupHeader(g_temp),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst_temp_hdr_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    if !cx.update(|cx| cx.has_active_drag()) {
        return Err("GPUI drag unexpectedly ended while hovering destination header".into());
    }

    // Cancel active drag via real Escape keystroke, then release mouse directly over valid header.
    dispatch_key(window, cx, "escape")?;
    cx.update(|cx| -> Result<(), String> {
        if cx.has_active_drag() {
            return Err("Escape keystroke did not stop active GPUI drag".into());
        }
        if workspace.read(cx).group_ui.drag_preview.is_some() {
            return Err("Escape keystroke did not clear drag_preview state".into());
        }
        Ok(())
    })?;

    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst_temp_hdr_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: dst_temp_hdr_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;

    if cx.update(|cx| workspace.read(cx).session_layout() != &layout_before_cancel) {
        return Err("MouseUp after Escape drag-cancel still mutated SessionLayout".into());
    }
    let drag_cancel_verified = true;

    // -------------------------------------------------------------------------
    // 3. Native Mouse Drag + Valid Drops (`drag_drop_verified`) & Remaining Modals
    // -------------------------------------------------------------------------
    // 3a. Drag active tab pill onto `g_temp` group header -> moves `v_active` into `g_temp`.
    let (src_tab_pt, viewport) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::TabPill(v_active.view_id),
    )?;
    let nudge_pt = drag_nudge_point(src_tab_pt, viewport);
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: src_tab_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: src_tab_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: nudge_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    if !cx.update(|cx| cx.has_active_drag()) {
        return Err("failed to start native tab drag for valid drop onto group header".into());
    }

    let (dst_temp_hdr_pt, _) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::GroupHeader(g_temp),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst_temp_hdr_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: dst_temp_hdr_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;

    cx.update(|cx| -> Result<(), String> {
        if cx.has_active_drag() {
            return Err("active drag remained set after MouseUp drop onto group header".into());
        }
        let ws = workspace.read(cx);
        let moved_grp = ws
            .session_layout()
            .view(v_active.view_id)
            .map(|v| v.group_id);
        if moved_grp != Some(g_temp) {
            return Err(format!(
                "tab drop onto group header did not move active tab to g_temp: got {moved_grp:?}"
            ));
        }
        if ws.active_view_id() != Some(v_active.view_id) {
            return Err("tab drop unexpectedly changed active_view_id".into());
        }
        Ok(())
    })?;

    // 3b. Exercise Rename modal, Collapse/Expand active group pill, and Recovery Browser via real keystrokes.
    focus_active_editor_or_workspace(window, workspace, cx)?;
    dispatch_key(window, cx, RENAME_GROUP_SHORTCUT)?;
    dispatch_ascii_text(window, cx, "ui-renamed-grp")?;
    dispatch_key(window, cx, "enter")?;
    settle_tick(window, cx).await?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws.group_ui.editor_modal.is_some() {
            return Err("Rename modal remained open after Enter keystroke".into());
        }
        let gname = ws.session_layout().group(g_temp).map(|g| g.name.as_str());
        if gname != Some("ui-renamed-grp") {
            return Err(format!("Rename modal did not update group name: {gname:?}"));
        }
        Ok(())
    })?;

    // Collapse & re-expand active custom group via shortcut (renders collapsed active pill!).
    dispatch_key(window, cx, TOGGLE_COLLAPSE_SHORTCUT)?;
    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        let collapsed = ws
            .session_layout()
            .group(g_temp)
            .is_some_and(|g| g.collapsed);
        if !collapsed || ws.active_view_id() != Some(v_active.view_id) {
            return Err("collapsing active group failed or altered active_view_id".into());
        }
        Ok(())
    })?;
    dispatch_key(window, cx, TOGGLE_COLLAPSE_SHORTCUT)?;
    if cx.update(|cx| {
        workspace
            .read(cx)
            .session_layout()
            .group(g_temp)
            .is_none_or(|g| g.collapsed)
    }) {
        return Err("second ToggleCurrentGroupCollapse keystroke did not expand group".into());
    }

    // Open Recovery Catalog Browser via shortcut and dismiss with Escape, verifying focus restoration.
    dispatch_key(window, cx, OPEN_RECOVERY_SHORTCUT)?;
    if !cx.update(|cx| workspace.read(cx).group_ui.recovery_modal.is_some()) {
        return Err("OpenRecoveryCatalog shortcut did not open RecoveryBrowserModal".into());
    }
    dispatch_key(window, cx, "escape")?;
    let recovery_focus_ok = window
        .update(cx, |_, window, cx| {
            let ws = workspace.read(cx);
            ws.group_ui.recovery_modal.is_none() && window.focused(cx) == pre_modal_focus
        })
        .map_err(|e| e.to_string())?;
    if !recovery_focus_ok {
        return Err(
            "Escape keystroke on RecoveryBrowserModal did not close modal and restore focus".into(),
        );
    }
    let keyboard_modal_verified = true;

    // 3c. Drag `g_temp` group header onto first group's header -> reorders `g_temp` to index 0.
    let first_existing_grp = orig_group_order[0];
    let (src_grp_pt, viewport) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::GroupHeader(g_temp),
    )?;
    let grp_nudge = drag_nudge_point(src_grp_pt, viewport);
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: src_grp_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: src_grp_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: grp_nudge,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    cx.update(|cx| -> Result<(), String> {
        if !cx.has_active_drag() {
            return Err("MouseDown+MouseMove on group header did not start GPUI group drag".into());
        }
        if workspace.read(cx).group_ui.drag_preview != Some(DragPreviewSession::Group(g_temp)) {
            return Err("group drag did not record DragPreviewSession::Group(g_temp)".into());
        }
        Ok(())
    })?;

    let (dst_first_grp_pt, _) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::GroupHeader(first_existing_grp),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst_first_grp_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: dst_first_grp_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;

    if cx.update(|cx| {
        workspace
            .read(cx)
            .session_layout()
            .tree
            .groups
            .first()
            .map(|g| g.id)
            != Some(g_temp)
    }) {
        return Err("group header drag-and-drop did not reorder g_temp to front".into());
    }

    // 3d. Drag `v_active` back to its exact original position in `orig_group` (before `orig_next_in_group`
    // via tab-pill drop when a successor exists, or onto `GroupHeader(orig_group)` when last/sole).
    let (src_tab_pt, viewport) = scroll_and_hit_point(
        window,
        workspace,
        cx,
        orig_pane,
        TabStripItem::TabPill(v_active.view_id),
    )?;
    let nudge_back = drag_nudge_point(src_tab_pt, viewport);
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: src_tab_pt,
            pressed_button: None,
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Left,
            position: src_tab_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: nudge_back,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    if !cx.update(|cx| cx.has_active_drag()) {
        return Err("failed to start tab drag back to original group".into());
    }

    let return_drop_item = match orig_next_in_group {
        Some(next_vid) => TabStripItem::TabPill(next_vid),
        None => TabStripItem::GroupHeader(orig_group),
    };
    let (dst_return_pt, _) =
        scroll_and_hit_point(window, workspace, cx, orig_pane, return_drop_item)?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: dst_return_pt,
            pressed_button: Some(MouseButton::Left),
            modifiers: Modifiers::default(),
        }),
    )?;
    dispatch_mouse(
        window,
        cx,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Left,
            position: dst_return_pt,
            modifiers: Modifiers::default(),
            click_count: 1,
        }),
    )?;

    cx.update(|cx| -> Result<(), String> {
        let ws = workspace.read(cx);
        if ws
            .session_layout()
            .view(v_active.view_id)
            .map(|v| v.group_id)
            != Some(orig_group)
        {
            return Err("tab drop did not restore v_active to orig_group".into());
        }
        if let Some(next_vid) = orig_next_in_group {
            let tabs = ws
                .pane_view_ids(orig_pane)
                .ok_or("orig_pane missing after return drop")?;
            let pos_a = tabs.iter().position(|&id| id == v_active.view_id);
            let pos_n = tabs.iter().position(|&id| id == next_vid);
            if pos_a.map(|i| i + 1) != pos_n {
                return Err(format!(
                    "tab-before-tab drop did not place active tab immediately before {next_vid:?}: pos_a={pos_a:?}, pos_n={pos_n:?}"
                ));
            }
        }
        if ws.active_view_id() != Some(v_active.view_id) {
            return Err("tab-before-tab drop activated an inactive view".into());
        }
        Ok(())
    })?;
    let drag_drop_verified = true;

    // -------------------------------------------------------------------------
    // 4. Clean Removal of `g_temp` & Full Invariant / Hydration / Text Audit
    // -------------------------------------------------------------------------
    window
        .update(cx, |_, window, cx| -> Result<(), String> {
            workspace.update(cx, |ws, cx| {
                ws.apply_group_command(GroupCommand::Remove { group: g_temp }, window, cx)
                    .map_err(|e| format!("cleanup Remove(g_temp) failed: {e}"))?;
                if orig_was_collapsed {
                    ws.apply_group_command(
                        GroupCommand::SetCollapsed {
                            group: orig_group,
                            collapsed: true,
                        },
                        window,
                        cx,
                    )
                    .map_err(|e| format!("cleanup restore collapsed failed: {e}"))?;
                }
                if let Some(scroll) = ws.group_ui.panes.strip_scroll(orig_pane) {
                    scroll.set_offset(orig_strip_offset);
                }
                Ok(())
            })
        })
        .map_err(|e| e.to_string())??;

    focus_active_editor_or_workspace(window, workspace, cx)?;
    settle_tick(window, cx).await?;

    cx.update(|cx| -> Result<(), String> {
        if cx.has_active_drag() {
            return Err("dangling GPUI active_drag after verify_group_ui_native".into());
        }
        let ws = workspace.read(cx);
        ws.session_layout()
            .validate()
            .map_err(|e| format!("post-smoke SessionLayout invalid: {e}"))?;
        if ws.session_layout() != &orig_layout {
            return Err(format!(
                "verify_group_ui_native did not restore exact initial SessionLayout:\nactual={:?}\nexpected={:?}",
                ws.session_layout(),
                orig_layout
            ));
        }
        if ws.hydrated_editor_count() != orig_hydrated_count {
            return Err(format!(
                "verify_group_ui_native changed hydrated_editor_count: before={orig_hydrated_count}, after={}",
                ws.hydrated_editor_count()
            ));
        }
        let after_tags = capture_slot_tags(ws);
        if after_tags != orig_slot_tags {
            return Err(
                "verify_group_ui_native altered Editable vs Dehydrated slot set".into(),
            );
        }
        let after_editables = capture_editable_snapshots(ws);
        if after_editables != orig_editables {
            return Err(format!(
                "verify_group_ui_native altered editor entity/revision/dirty/undo/text state:\nbefore={orig_editables:?}\nafter={after_editables:?}"
            ));
        }
        Ok(())
    })?;

    Ok(GroupUiNativeEvidence {
        drag_drop_verified,
        drag_cancel_verified,
        keyboard_modal_verified,
    })
}
