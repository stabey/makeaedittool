use std::time::Instant;

use editor_services::{
    ErrorKind, GroupColor, GroupCommand, GroupCommandResult, GroupId, MAX_CUSTOM_GROUPS,
    MAX_GROUP_NAME_BYTES, MAX_PANES, MAX_SESSION_VIEWS, PaneCommand, PaneId, PaneMove,
    RecoveryCursor, RecoveryEntry, RecoveryEntryCategory, RecoveryEntryKey, RecoveryEntryOrigin,
    ServiceError, SessionLayout, SplitDirection, ViewId, ViewTarget,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputEvent, InputState};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, StyledExt as _, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, ElementId, Entity, EntityId, FocusHandle, Hsla,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Render, Role,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window,
    black, div, hsla, prelude::FluentBuilder as _, px,
};

use crate::actions::{
    CloseCurrentGroupTabs, CreateEmptyGroup, CreateGroupFromActiveTab, CycleCurrentGroupColor,
    MoveActiveTabEarlierInGroup, MoveActiveTabLaterInGroup, MoveActiveTabToNextGroup,
    MoveActiveTabToPrevGroup, MoveCurrentGroupEarlier, MoveCurrentGroupLater, OpenRecoveryCatalog,
    RemoveCurrentGroupKeepTabs, RenameCurrentGroup, ToggleCurrentGroupCollapse, UngroupActiveTab,
    WORKSPACE_CONTEXT,
};
use crate::app::{EditorWorkspace, OverlayOrigin, TabSlot};
use crate::paged_view::format_bytes;

#[path = "pane_ui_smoke.rs"]
pub(crate) mod pane_smoke;
#[path = "pane_ui.rs"]
pub(crate) mod pane_ui;
#[path = "group_ui_smoke.rs"]
pub(crate) mod smoke;
pub(crate) use pane_smoke::verify_pane_ui_native;
pub(crate) use smoke::verify_group_ui_native;

/// UI-owned Command Palette action payload routed via `PaletteCommandTarget::GroupUi`.
/// Carries explicit `ViewTarget` and `GroupId` identities — never mutable row indices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupPaletteAction {
    PromptCreateGroup { initial_view: Option<ViewTarget> },
    PromptRenameGroup { group: GroupId },
    SetGroupColor { group: GroupId, color: GroupColor },
    SetGroupCollapsed { group: GroupId, collapsed: bool },
    MoveViewToGroup { target: ViewTarget, group: GroupId },
    MoveViewEarlierInGroup { target: ViewTarget },
    MoveViewLaterInGroup { target: ViewTarget },
    MoveGroupEarlier { group: GroupId },
    MoveGroupLater { group: GroupId },
    RemoveGroupKeepTabs { group: GroupId },
    CloseGroupTabs { group: GroupId },
    OpenRecoveryBrowser,
    RetryHydration { target: ViewTarget },
    ExportDraftCopy { target: ViewTarget },
    Pane(pane_ui::PanePaletteAction),
}

/// Ephemeral ID-only drag payload for tab membership and within-group / cross-group reordering.
/// Never clones document text, `TabSlot`, or `SessionDocumentStub`, and never mutates session
/// state until a valid drop completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DraggedTabView {
    pub(crate) workspace_entity_id: EntityId,
    pub(crate) source_pane: PaneId,
    pub(crate) target: ViewTarget,
}

/// Ephemeral ID-only drag payload for global group reordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DraggedTabGroup {
    pub(crate) workspace_entity_id: EntityId,
    pub(crate) group_id: GroupId,
}

/// Floating native drag ghost for a dragged tab; resolves live title/color via `WeakEntity`.
pub(crate) struct DraggedTabGhost {
    pub(crate) workspace: WeakEntity<EditorWorkspace>,
    pub(crate) target: ViewTarget,
}

impl Render for DraggedTabGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let (title, color_dot) = self
            .workspace
            .upgrade()
            .and_then(|ws| {
                let ws_ref = ws.read(cx);
                let lv = ws_ref.session_layout().resolve(self.target).ok()?;
                let group_color = ws_ref
                    .session_layout()
                    .group(lv.group_id)
                    .map(|g| g.color)
                    .unwrap_or(GroupColor::Neutral);
                let title = ws_ref.view_slot(self.target.view_id)?.title().to_owned();
                Some((title, group_color_hsla(group_color, is_dark)))
            })
            .unwrap_or_else(|| {
                (
                    "Tab".to_owned(),
                    group_color_hsla(GroupColor::Neutral, is_dark),
                )
            });

        h_flex()
            .px_2p5()
            .py_1()
            .gap_1p5()
            .items_center()
            .max_w(px(220.))
            .rounded(px(6.))
            .border_1()
            .border_color(cx.theme().drag_border)
            .bg(cx.theme().popover.opacity(0.95))
            .text_color(cx.theme().popover_foreground)
            .shadow_md()
            .text_xs()
            .font_medium()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(color_dot),
            )
            .child(div().truncate().child(title))
    }
}

/// Floating native drag ghost for a dragged tab group; resolves live group metadata via `WeakEntity`.
pub(crate) struct DraggedGroupGhost {
    pub(crate) workspace: WeakEntity<EditorWorkspace>,
    pub(crate) group_id: GroupId,
}

impl Render for DraggedGroupGhost {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_dark = cx.theme().mode.is_dark();
        let (name, color_dot) = self
            .workspace
            .upgrade()
            .and_then(|ws| {
                let ws_ref = ws.read(cx);
                let grp = ws_ref.session_layout().group(self.group_id)?;
                Some((grp.name.clone(), group_color_hsla(grp.color, is_dark)))
            })
            .unwrap_or_else(|| {
                (
                    "Group".to_owned(),
                    group_color_hsla(GroupColor::Neutral, is_dark),
                )
            });

        h_flex()
            .px_2p5()
            .py_1()
            .gap_1p5()
            .items_center()
            .max_w(px(220.))
            .rounded(px(6.))
            .border_1()
            .border_color(cx.theme().drag_border)
            .bg(cx.theme().popover.opacity(0.95))
            .text_color(cx.theme().popover_foreground)
            .shadow_md()
            .text_xs()
            .font_semibold()
            .child(
                div()
                    .size(px(9.))
                    .rounded(px(2.))
                    .flex_shrink_0()
                    .bg(color_dot),
            )
            .child(div().truncate().child(name))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragPreviewSession {
    Tab {
        source_pane: PaneId,
        target: ViewTarget,
    },
    Group(GroupId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupEditorModalKind {
    Create {
        before: Option<GroupId>,
        initial_view: Option<ViewTarget>,
        cross_pane_move: Option<(PaneId, PaneId)>,
    },
    Rename {
        group: GroupId,
    },
}

struct GroupEditorModal {
    kind: GroupEditorModalKind,
    origin: OverlayOrigin,
    name_input: Entity<InputState>,
    selected_color: GroupColor,
    validation_error: Option<String>,
    _input_sub: Subscription,
}

struct RecoveryBrowserModal {
    origin: OverlayOrigin,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    cursor_history: Vec<Option<RecoveryCursor>>,
    current_cursor: Option<RecoveryCursor>,
}

/// UI-owned transient state stored on `EditorWorkspace.group_ui`.
#[derive(Default)]
pub(crate) struct GroupUiState {
    pub(crate) drag_preview: Option<DragPreviewSession>,
    editor_modal: Option<GroupEditorModal>,
    recovery_modal: Option<RecoveryBrowserModal>,
    dismissed_overflow_banner_for_total: Option<u64>,
    pub(crate) panes: pane_ui::PaneUiState,
}

impl GroupUiState {
    #[inline]
    pub(crate) fn sync_drag_liveness(&mut self, cx: &App) {
        if self.drag_preview.is_some() && !cx.has_active_drag() {
            self.drag_preview = None;
        }
    }

    #[inline]
    pub(crate) fn is_dragging_tab(&self, cx: &App) -> bool {
        matches!(self.drag_preview, Some(DragPreviewSession::Tab { .. })) && cx.has_active_drag()
    }

    #[inline]
    pub(crate) fn is_overflow_banner_dismissed(&self, unplaced_total: u64, deferred: u64) -> bool {
        let sig = unplaced_total
            .wrapping_mul(1_000_003)
            .wrapping_add(deferred);
        self.dismissed_overflow_banner_for_total == Some(sig)
    }

    #[inline]
    pub(crate) fn dismiss_overflow_banner(&mut self, unplaced_total: u64, deferred: u64) {
        let sig = unplaced_total
            .wrapping_mul(1_000_003)
            .wrapping_add(deferred);
        self.dismissed_overflow_banner_for_total = Some(sig);
    }
}

/// Builds a stack-only, collision-free `ElementId` from a 4-byte tag and a 128-bit `ViewId`.
#[inline]
pub(crate) fn view_elem_id(tag: [u8; 4], id: ViewId) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4..].copy_from_slice(&id.0.to_be_bytes());
    ElementId::from(bytes)
}

/// Builds a stack-only, collision-free `ElementId` from a 4-byte tag and a 128-bit `GroupId`.
#[inline]
pub(crate) fn group_elem_id(tag: [u8; 4], id: GroupId) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4..].copy_from_slice(&id.0.to_be_bytes());
    ElementId::from(bytes)
}

#[inline]
pub(crate) fn recovery_origin_slug(origin: RecoveryEntryOrigin) -> &'static str {
    match origin {
        RecoveryEntryOrigin::Document => "doc",
        RecoveryEntryOrigin::LegacyOrphan => "legacy-orphan",
    }
}

/// Builds a stack-only 20-byte `ElementId` for a `RecoveryEntryKey`:
/// `[tag: 4B][origin: 1B][signed rowid: 8B][zero padding: 7B]`.
/// `(origin, rowid)` uniquely identifies the SQLite source record across `i64::MIN..=i64::MAX`,
/// while full `RecoveryEntryKey` (including 64-bit `lifecycle_gen`) stays in callbacks and Core validation.
#[inline]
pub(crate) fn recovery_elem_id(tag: [u8; 4], key: RecoveryEntryKey) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4] = match key.origin {
        RecoveryEntryOrigin::Document => 1,
        RecoveryEntryOrigin::LegacyOrphan => 2,
    };
    bytes[5..13].copy_from_slice(&key.rowid.to_be_bytes());
    ElementId::from(bytes)
}

/// Accessible high-contrast color token for `GroupColor` in light and dark themes.
#[inline]
pub(crate) fn group_color_hsla(color: GroupColor, is_dark: bool) -> Hsla {
    match color {
        GroupColor::Neutral => hsla(0.0, 0.0, if is_dark { 0.62 } else { 0.44 }, 1.0),
        GroupColor::Red => hsla(0.0, 0.78, if is_dark { 0.62 } else { 0.48 }, 1.0),
        GroupColor::Orange => hsla(0.075, 0.85, if is_dark { 0.60 } else { 0.46 }, 1.0),
        GroupColor::Yellow => hsla(0.125, 0.86, if is_dark { 0.56 } else { 0.38 }, 1.0),
        GroupColor::Green => hsla(0.39, 0.68, if is_dark { 0.52 } else { 0.36 }, 1.0),
        GroupColor::Teal => hsla(0.49, 0.72, if is_dark { 0.52 } else { 0.36 }, 1.0),
        GroupColor::Blue => hsla(0.59, 0.82, if is_dark { 0.63 } else { 0.48 }, 1.0),
        GroupColor::Purple => hsla(0.75, 0.74, if is_dark { 0.66 } else { 0.50 }, 1.0),
        GroupColor::Pink => hsla(0.91, 0.78, if is_dark { 0.64 } else { 0.48 }, 1.0),
    }
}

#[inline]
pub(crate) fn next_group_color(current: GroupColor) -> GroupColor {
    let idx = GroupColor::ALL
        .iter()
        .position(|&c| c == current)
        .unwrap_or(0);
    GroupColor::ALL[(idx + 1) % GroupColor::ALL.len()]
}

/// Validates a custom group name draft without mutating state (mirrors `SessionLayout` rules).
pub(crate) fn validate_group_name_draft(raw: &str) -> Result<(&str, usize), &'static str> {
    if raw.chars().any(char::is_control) {
        return Err("Group name cannot contain control characters.");
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Group name cannot be empty.");
    }
    if trimmed.len() > MAX_GROUP_NAME_BYTES {
        return Err("Group name cannot exceed 128 UTF-8 bytes.");
    }
    Ok((trimmed, trimmed.len()))
}

/// Generates a concise default name and rotating color for a newly created custom group.
pub(crate) fn suggest_new_group_defaults(layout: &SessionLayout) -> (String, GroupColor) {
    let custom_count = layout
        .tree
        .groups
        .iter()
        .filter(|g| g.id != GroupId::UNGROUPED)
        .count();
    let number = custom_count + 1;
    let non_neutral = &GroupColor::ALL[1..];
    let color = non_neutral[custom_count % non_neutral.len()];
    (format!("Group {number}"), color)
}

/// Computes `(group_id, before_view)` to move `target` one slot earlier inside its current group.
pub(crate) fn prev_view_anchor_in_group(
    layout: &SessionLayout,
    target: ViewTarget,
) -> Option<(GroupId, Option<ViewId>)> {
    let lv = layout.resolve(target).ok()?;
    let group = lv.group_id;
    let pane_id = layout.pane_for_view(target.view_id)?;
    let members: Vec<ViewId> = layout
        .pane(pane_id)?
        .tabs
        .iter()
        .copied()
        .filter(|id| layout.view(*id).is_some_and(|v| v.group_id == group))
        .collect();
    let pos = members.iter().position(|&id| id == target.view_id)?;
    if pos == 0 {
        None
    } else {
        Some((group, Some(members[pos - 1])))
    }
}

/// Computes `(group_id, before_view)` to move `target` one slot later inside its current group.
pub(crate) fn next_view_anchor_in_group(
    layout: &SessionLayout,
    target: ViewTarget,
) -> Option<(GroupId, Option<ViewId>)> {
    let lv = layout.resolve(target).ok()?;
    let group = lv.group_id;
    let pane_id = layout.pane_for_view(target.view_id)?;
    let members: Vec<ViewId> = layout
        .pane(pane_id)?
        .tabs
        .iter()
        .copied()
        .filter(|id| layout.view(*id).is_some_and(|v| v.group_id == group))
        .collect();
    let pos = members.iter().position(|&id| id == target.view_id)?;
    if pos + 1 >= members.len() {
        None
    } else if pos + 2 < members.len() {
        Some((group, Some(members[pos + 2])))
    } else {
        Some((group, None))
    }
}

/// Computes `before: Option<GroupId>` to move `group` one slot earlier in global group order.
pub(crate) fn prev_group_anchor(layout: &SessionLayout, group: GroupId) -> Option<Option<GroupId>> {
    let pos = layout.tree.groups.iter().position(|g| g.id == group)?;
    if pos == 0 {
        None
    } else {
        Some(Some(layout.tree.groups[pos - 1].id))
    }
}

/// Computes `before: Option<GroupId>` to move `group` one slot later in global group order.
pub(crate) fn next_group_anchor(layout: &SessionLayout, group: GroupId) -> Option<Option<GroupId>> {
    let len = layout.tree.groups.len();
    let pos = layout.tree.groups.iter().position(|g| g.id == group)?;
    if pos + 1 >= len {
        None
    } else if pos + 2 < len {
        Some(Some(layout.tree.groups[pos + 2].id))
    } else {
        Some(None)
    }
}

/// Resolves the next or previous `GroupId` in global group order (wrapping around).
pub(crate) fn adjacent_group_id(
    layout: &SessionLayout,
    current: GroupId,
    forward: bool,
) -> Option<GroupId> {
    let len = layout.tree.groups.len();
    if len <= 1 {
        return None;
    }
    let pos = layout.tree.groups.iter().position(|g| g.id == current)?;
    let next_pos = if forward {
        (pos + 1) % len
    } else {
        (pos + len - 1) % len
    };
    Some(layout.tree.groups[next_pos].id)
}

impl EditorWorkspace {
    /// Contract entrypoint: cancels active group/tab drag or dismisses Group UI modals/browser
    /// before Core's Escape/overlay-switch handlers run. Never touches Core's lifecycle dialogs.
    pub(crate) fn cancel_or_dismiss_group_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.cancel_divider_drag(cx) {
            return true;
        }
        let stopped_gpui_drag = cx.stop_active_drag(window);
        let had_drag_preview = self.group_ui.drag_preview.take().is_some();
        if stopped_gpui_drag || had_drag_preview {
            self.status_flash = Some(("Drag cancelled".to_string(), Instant::now()));
            cx.notify();
            return true;
        }

        if let Some(modal) = self.group_ui.editor_modal.take() {
            self.restore_overlay_origin(modal.origin, window, cx);
            cx.notify();
            return true;
        }

        if let Some(browser) = self.group_ui.recovery_modal.take() {
            if self.recovery_browser_state().loading
                || self.recovery_browser_state().active_export.is_some()
            {
                self.cancel_recovery_request(cx);
            }
            self.restore_overlay_origin(browser.origin, window, cx);
            cx.notify();
            return true;
        }

        false
    }

    /// Contract entrypoint: executes a `GroupPaletteAction` selected from Command Palette,
    /// preserving `origin` across modal openings and focus restoration.
    pub(crate) fn execute_group_palette_action(
        &mut self,
        action: GroupPaletteAction,
        origin: OverlayOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            GroupPaletteAction::PromptCreateGroup { initial_view } => {
                self.open_create_group_modal(initial_view, None, None, Some(origin), window, cx);
            }
            GroupPaletteAction::PromptRenameGroup { group } => {
                self.open_rename_group_modal(group, Some(origin), window, cx);
            }
            GroupPaletteAction::SetGroupColor { group, color } => {
                let _ =
                    self.run_ui_group_command(GroupCommand::SetColor { group, color }, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::SetGroupCollapsed { group, collapsed } => {
                let _ = self.run_ui_group_command(
                    GroupCommand::SetCollapsed { group, collapsed },
                    window,
                    cx,
                );
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::MoveViewToGroup { target, group } => {
                let _ = self.run_ui_group_command(
                    GroupCommand::MoveView {
                        target,
                        group,
                        before: None,
                    },
                    window,
                    cx,
                );
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::MoveViewEarlierInGroup { target } => {
                self.move_view_step_in_group(target, false, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::MoveViewLaterInGroup { target } => {
                self.move_view_step_in_group(target, true, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::MoveGroupEarlier { group } => {
                self.move_group_step(group, false, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::MoveGroupLater { group } => {
                self.move_group_step(group, true, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::RemoveGroupKeepTabs { group } => {
                let _ = self.run_ui_group_command(GroupCommand::Remove { group }, window, cx);
                self.restore_non_editor_palette_focus(origin, window, cx);
            }
            GroupPaletteAction::CloseGroupTabs { group } => {
                self.request_close_group(group, window, cx);
            }
            GroupPaletteAction::OpenRecoveryBrowser => {
                self.open_recovery_browser(Some(origin), window, cx);
            }
            GroupPaletteAction::RetryHydration { target } => {
                self.retry_view_hydration(target, window, cx);
            }
            GroupPaletteAction::ExportDraftCopy { target } => {
                self.export_view_draft_copy(target, window, cx);
            }
            GroupPaletteAction::Pane(pane_action) => {
                self.execute_pane_palette_action(pane_action, origin, window, cx);
            }
        }
    }

    fn restore_non_editor_palette_focus(
        &mut self,
        origin: OverlayOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(
            origin.focus,
            crate::app::OverlayFocus::SearchQuery | crate::app::OverlayFocus::SearchReplace
        ) {
            self.restore_overlay_origin(origin, window, cx);
        }
    }

    #[inline]
    pub(crate) fn is_recovery_browser_open(&self) -> bool {
        self.group_ui.recovery_modal.is_some()
    }

    #[inline]
    pub(crate) fn clear_drag_preview(&mut self) {
        self.group_ui.drag_preview = None;
    }

    pub(crate) fn record_tab_drag_start(
        &mut self,
        source_pane: PaneId,
        target: ViewTarget,
        cx: &mut Context<Self>,
    ) {
        self.group_ui.drag_preview = Some(DragPreviewSession::Tab {
            source_pane,
            target,
        });
        cx.notify();
    }

    pub(crate) fn record_group_drag_start(&mut self, group_id: GroupId, cx: &mut Context<Self>) {
        self.group_ui.drag_preview = Some(DragPreviewSession::Group(group_id));
        cx.notify();
    }

    pub(crate) fn complete_tab_drop_before_view(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        dest_group: GroupId,
        before_view: ViewId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group_ui.drag_preview = None;
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        if drag.target.view_id == before_view {
            cx.notify();
            return;
        }
        if drag.source_pane == dest_pane {
            let _ = self.run_ui_group_command(
                GroupCommand::MoveView {
                    target: drag.target,
                    group: dest_group,
                    before: Some(before_view),
                },
                window,
                cx,
            );
        } else {
            let _ = self.run_ui_pane_command(
                PaneCommand::MoveView {
                    target: drag.target,
                    source: drag.source_pane,
                    destination: dest_pane,
                    group: dest_group,
                    before: Some(before_view),
                },
                window,
                cx,
            );
        }
    }

    pub(crate) fn complete_tab_drop_into_group(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        dest_group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group_ui.drag_preview = None;
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        if drag.source_pane == dest_pane {
            let _ = self.run_ui_group_command(
                GroupCommand::MoveView {
                    target: drag.target,
                    group: dest_group,
                    before: None,
                },
                window,
                cx,
            );
        } else {
            let _ = self.run_ui_pane_command(
                PaneCommand::MoveView {
                    target: drag.target,
                    source: drag.source_pane,
                    destination: dest_pane,
                    group: dest_group,
                    before: None,
                },
                window,
                cx,
            );
        }
    }

    pub(crate) fn complete_tab_drop_new_group(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group_ui.drag_preview = None;
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        let cross_pane = (drag.source_pane != dest_pane).then_some((drag.source_pane, dest_pane));
        self.open_create_group_modal(Some(drag.target), None, cross_pane, None, window, cx);
    }

    pub(crate) fn complete_group_drop_before(
        &mut self,
        drag: DraggedTabGroup,
        before_group: Option<GroupId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.group_ui.drag_preview = None;
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        if before_group == Some(drag.group_id) {
            cx.notify();
            return;
        }
        let _ = self.run_ui_group_command(
            GroupCommand::Reorder {
                group: drag.group_id,
                before: before_group,
            },
            window,
            cx,
        );
    }

    pub(crate) fn run_ui_group_command(
        &mut self,
        command: GroupCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<GroupCommandResult> {
        match self.apply_group_command(command, window, cx) {
            Ok(res) => Some(res),
            Err(err) => {
                self.notify_group_service_error(&err, window, cx);
                None
            }
        }
    }

    pub(crate) fn notify_group_service_error(
        &mut self,
        err: &ServiceError,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let msg = match err.kind {
            ErrorKind::Busy => format!(
                "Workspace busy ({}): wait for the active operation to complete.",
                err.message
            ),
            ErrorKind::Stale => format!("Target changed before command ran: {}", err.message),
            ErrorKind::InvalidInput => err.message.clone(),
            _ => format!("Group layout error: {}", err.message),
        };
        self.status_flash = Some((msg.clone(), Instant::now()));
        match err.kind {
            ErrorKind::Storage | ErrorKind::Corrupt | ErrorKind::Unsupported => {
                window.push_notification(Notification::error(msg), cx);
            }
            _ => {
                window.push_notification(Notification::warning(msg), cx);
            }
        }
        cx.notify();
    }

    pub(crate) fn open_create_group_modal(
        &mut self,
        initial_view: Option<ViewTarget>,
        before: Option<GroupId>,
        cross_pane_move: Option<(PaneId, PaneId)>,
        explicit_origin: Option<OverlayOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_lifecycle_busy() {
            let msg = "Cannot create a tab group while a close or reload operation is in progress.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::warning(msg), cx);
            cx.notify();
            return;
        }

        let custom_count = self
            .session_layout()
            .tree
            .groups
            .iter()
            .filter(|g| g.id != GroupId::UNGROUPED)
            .count();
        if custom_count >= MAX_CUSTOM_GROUPS {
            let msg = format!("Maximum of {MAX_CUSTOM_GROUPS} custom tab groups reached.");
            self.status_flash = Some((msg.clone(), Instant::now()));
            window.push_notification(Notification::warning(msg), cx);
            cx.notify();
            return;
        }

        let origin = explicit_origin.unwrap_or_else(|| self.capture_overlay_origin(window, cx));
        // Dismiss any existing recovery modal without restoring focus mid-switch.
        self.group_ui.recovery_modal = None;

        let (default_name, default_color) = suggest_new_group_defaults(self.session_layout());
        let name_len = default_name.len();
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name (1–128 UTF-8 bytes)")
                .default_value(default_name)
        });
        let sub = cx.subscribe_in(
            &name_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    if let Some(modal) = &mut this.group_ui.editor_modal {
                        modal.validation_error = None;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.submit_group_editor_modal(window, cx);
                }
                _ => {}
            },
        );
        name_input.update(cx, |state, cx| {
            state.set_selected_range(0..name_len, cx);
            state.focus(window, cx);
        });

        self.group_ui.editor_modal = Some(GroupEditorModal {
            kind: GroupEditorModalKind::Create {
                before,
                initial_view,
                cross_pane_move,
            },
            origin,
            name_input,
            selected_color: default_color,
            validation_error: None,
            _input_sub: sub,
        });
        cx.notify();
    }

    pub(crate) fn open_rename_group_modal(
        &mut self,
        group: GroupId,
        explicit_origin: Option<OverlayOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if group == GroupId::UNGROUPED {
            let msg = "Ungrouped has a fixed name and color; create a custom group instead.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg), cx);
            cx.notify();
            return;
        }
        if self.is_lifecycle_busy() {
            let msg = "Cannot rename a tab group while a close or reload operation is in progress.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::warning(msg), cx);
            cx.notify();
            return;
        }

        let Some(existing) = self.session_layout().group(group) else {
            window.push_notification(Notification::warning("Tab group no longer exists."), cx);
            return;
        };
        let current_name = existing.name.clone();
        let current_color = existing.color;
        let name_len = current_name.len();

        let origin = explicit_origin.unwrap_or_else(|| self.capture_overlay_origin(window, cx));
        self.group_ui.recovery_modal = None;

        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group name (1–128 UTF-8 bytes)")
                .default_value(current_name)
        });
        let sub = cx.subscribe_in(
            &name_input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => {
                    if let Some(modal) = &mut this.group_ui.editor_modal {
                        modal.validation_error = None;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => {
                    this.submit_group_editor_modal(window, cx);
                }
                _ => {}
            },
        );
        name_input.update(cx, |state, cx| {
            state.set_selected_range(0..name_len, cx);
            state.focus(window, cx);
        });

        self.group_ui.editor_modal = Some(GroupEditorModal {
            kind: GroupEditorModalKind::Rename { group },
            origin,
            name_input,
            selected_color: current_color,
            validation_error: None,
            _input_sub: sub,
        });
        cx.notify();
    }

    pub(crate) fn dismiss_group_editor_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(modal) = self.group_ui.editor_modal.take() {
            self.restore_overlay_origin(modal.origin, window, cx);
            cx.notify();
        }
    }

    pub(crate) fn submit_group_editor_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((kind, origin, raw_name, selected_color, name_input)) =
            self.group_ui.editor_modal.as_ref().map(|m| {
                (
                    m.kind,
                    m.origin,
                    m.name_input.read(cx).value().to_string(),
                    m.selected_color,
                    m.name_input.clone(),
                )
            })
        else {
            return false;
        };

        if let Err(msg) = validate_group_name_draft(&raw_name) {
            if let Some(modal) = &mut self.group_ui.editor_modal {
                modal.validation_error = Some(msg.to_string());
            }
            window.push_notification(Notification::warning(msg), cx);
            name_input.update(cx, |state, cx| state.focus(window, cx));
            cx.notify();
            return false;
        }

        let res = match kind {
            GroupEditorModalKind::Create {
                before,
                initial_view,
                cross_pane_move,
            } => {
                // If initial_view became stale while modal was open, surface Stale via Core.
                let create_res = self.apply_group_command(
                    GroupCommand::Create {
                        name: raw_name,
                        color: selected_color,
                        before,
                        initial_view,
                    },
                    window,
                    cx,
                );
                if let Ok(ok_res) = &create_res
                    && let (Some(target), Some((src_pane, dst_pane)), Some(created_gid)) =
                        (initial_view, cross_pane_move, ok_res.created_group)
                    && src_pane != dst_pane
                {
                    if let Err(mv_err) = self.apply_pane_command(
                        PaneCommand::MoveView {
                            target,
                            source: src_pane,
                            destination: dst_pane,
                            group: created_gid,
                            before: None,
                        },
                        window,
                        cx,
                    ) {
                        self.notify_group_service_error(&mv_err, window, cx);
                    } else {
                        self.group_ui.editor_modal = None;
                        cx.notify();
                        return true;
                    }
                }
                create_res
            }
            GroupEditorModalKind::Rename { group } => {
                let prior = self
                    .session_layout()
                    .group(group)
                    .map(|g| (g.name.clone(), g.color));
                let trimmed_target = raw_name.trim().to_string();
                if let Some((old_name, old_color)) = &prior
                    && old_name == &trimmed_target
                {
                    if *old_color != selected_color {
                        self.apply_group_command(
                            GroupCommand::SetColor {
                                group,
                                color: selected_color,
                            },
                            window,
                            cx,
                        )
                    } else {
                        Ok(GroupCommandResult {
                            changed: false,
                            created_group: None,
                        })
                    }
                } else {
                    match self.apply_group_command(
                        GroupCommand::Rename {
                            group,
                            name: raw_name,
                        },
                        window,
                        cx,
                    ) {
                        Ok(rename_res) => {
                            if prior.is_some_and(|(_, c)| c != selected_color) {
                                let _ = self.apply_group_command(
                                    GroupCommand::SetColor {
                                        group,
                                        color: selected_color,
                                    },
                                    window,
                                    cx,
                                );
                            }
                            Ok(rename_res)
                        }
                        Err(err) => Err(err),
                    }
                }
            }
        };

        match res {
            Ok(_) => {
                self.group_ui.editor_modal = None;
                self.restore_overlay_origin(origin, window, cx);
                cx.notify();
                true
            }
            Err(err) if err.kind == ErrorKind::InvalidInput => {
                if let Some(modal) = &mut self.group_ui.editor_modal {
                    modal.validation_error = Some(err.message.clone());
                }
                window.push_notification(Notification::warning(err.message), cx);
                name_input.update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
                false
            }
            Err(err) => {
                self.group_ui.editor_modal = None;
                self.notify_group_service_error(&err, window, cx);
                self.restore_overlay_origin(origin, window, cx);
                false
            }
        }
    }

    pub(crate) fn open_recovery_browser(
        &mut self,
        explicit_origin: Option<OverlayOrigin>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let origin = explicit_origin
            .or_else(|| self.group_ui.recovery_modal.as_ref().map(|m| m.origin))
            .unwrap_or_else(|| self.capture_overlay_origin(window, cx));
        self.group_ui.editor_modal = None;

        let focus_handle = self
            .group_ui
            .recovery_modal
            .as_ref()
            .map(|m| m.focus_handle.clone())
            .unwrap_or_else(|| cx.focus_handle());
        focus_handle.focus(window, cx);

        self.group_ui.recovery_modal = Some(RecoveryBrowserModal {
            origin,
            focus_handle,
            scroll_handle: ScrollHandle::new(),
            cursor_history: Vec::new(),
            current_cursor: None,
        });
        self.request_recovery_page(None, window, cx);
        cx.notify();
    }

    pub(crate) fn dismiss_recovery_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(browser) = self.group_ui.recovery_modal.take() {
            if self.recovery_browser_state().loading
                || self.recovery_browser_state().active_export.is_some()
            {
                self.cancel_recovery_request(cx);
            }
            self.restore_overlay_origin(browser.origin, window, cx);
            cx.notify();
        }
    }

    /// Called by Core (`session_core.rs`) strictly after `open_recovery_entry` /
    /// `admit_recovered_view` activates an existing view or admits a recovered view.
    /// Leaves any active background export intact while cancelling only a pending page load.
    pub(crate) fn close_recovery_browser_on_open(&mut self, cx: &mut Context<Self>) {
        if self.group_ui.recovery_modal.take().is_some() {
            if self.recovery_browser_state().loading
                && self.recovery_browser_state().active_export.is_none()
            {
                self.cancel_recovery_request(cx);
            }
            cx.notify();
        }
    }

    fn navigate_recovery_next_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next_cursor) = self
            .recovery_browser_state()
            .page
            .as_ref()
            .and_then(|p| p.next)
        else {
            return;
        };
        let seq_before = self.recovery_browser_state().request_seq;
        self.request_recovery_page(Some(next_cursor), window, cx);
        if self.recovery_browser_state().request_seq != seq_before
            && let Some(modal) = &mut self.group_ui.recovery_modal
        {
            let prev = modal.current_cursor;
            modal.cursor_history.push(prev);
            modal.current_cursor = Some(next_cursor);
            modal.scroll_handle = ScrollHandle::new();
        }
        cx.notify();
    }

    fn navigate_recovery_prev_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(prev_cursor) = self
            .group_ui
            .recovery_modal
            .as_ref()
            .and_then(|m| m.cursor_history.last().copied())
        else {
            return;
        };
        let seq_before = self.recovery_browser_state().request_seq;
        self.request_recovery_page(prev_cursor, window, cx);
        if self.recovery_browser_state().request_seq != seq_before
            && let Some(modal) = &mut self.group_ui.recovery_modal
        {
            modal.cursor_history.pop();
            modal.current_cursor = prev_cursor;
            modal.scroll_handle = ScrollHandle::new();
        }
        cx.notify();
    }

    fn refresh_recovery_first_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let seq_before = self.recovery_browser_state().request_seq;
        self.request_recovery_page(None, window, cx);
        if self.recovery_browser_state().request_seq != seq_before
            && let Some(modal) = &mut self.group_ui.recovery_modal
        {
            modal.cursor_history.clear();
            modal.current_cursor = None;
            modal.scroll_handle = ScrollHandle::new();
        }
        cx.notify();
    }

    fn retry_current_recovery_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self
            .group_ui
            .recovery_modal
            .as_ref()
            .and_then(|m| m.current_cursor);
        self.request_recovery_page(cur, window, cx);
        cx.notify();
    }

    pub(crate) fn move_view_step_in_group(
        &mut self,
        target: ViewTarget,
        later: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = if later {
            next_view_anchor_in_group(self.session_layout(), target)
        } else {
            prev_view_anchor_in_group(self.session_layout(), target)
        };
        let Some((group, before)) = step else {
            let edge = if later { "end" } else { "start" };
            self.status_flash =
                Some((format!("Tab is already at {edge} of group"), Instant::now()));
            cx.notify();
            return;
        };
        let _ = self.run_ui_group_command(
            GroupCommand::MoveView {
                target,
                group,
                before,
            },
            window,
            cx,
        );
    }

    pub(crate) fn move_group_step(
        &mut self,
        group: GroupId,
        later: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = if later {
            next_group_anchor(self.session_layout(), group)
        } else {
            prev_group_anchor(self.session_layout(), group)
        };
        let Some(before) = step else {
            let edge = if later { "last" } else { "first" };
            self.status_flash = Some((format!("Group is already {edge}"), Instant::now()));
            cx.notify();
            return;
        };
        let _ = self.run_ui_group_command(GroupCommand::Reorder { group, before }, window, cx);
    }

    pub(crate) fn create_new_tab_in_group(
        &mut self,
        pane_id: PaneId,
        group: GroupId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_pane_id() != pane_id
            && let Err(err) = self.activate_pane(pane_id, window, cx)
        {
            self.notify_group_service_error(&err, window, cx);
            return;
        }
        let Some(created_target) = self.create_untitled_tab(true, window, cx) else {
            return;
        };
        let already_in_group = self
            .session_layout()
            .view(created_target.view_id)
            .is_some_and(|v| v.group_id == group);
        if !already_in_group {
            let _ = self.run_ui_group_command(
                GroupCommand::MoveView {
                    target: created_target,
                    group,
                    before: None,
                },
                window,
                cx,
            );
        }
    }

    // --- Keyboard & Native Menu Action Listeners ---

    pub(crate) fn on_create_group_from_active_tab(
        &mut self,
        _: &CreateGroupFromActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = self.active_view_id().and_then(|id| self.view_target(id));
        self.open_create_group_modal(initial, None, None, None, window, cx);
    }

    pub(crate) fn on_create_empty_group(
        &mut self,
        _: &CreateEmptyGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_create_group_modal(None, None, None, None, window, cx);
    }

    pub(crate) fn on_rename_current_group(
        &mut self,
        _: &RenameCurrentGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        self.open_rename_group_modal(group, None, window, cx);
    }

    pub(crate) fn on_cycle_current_group_color(
        &mut self,
        _: &CycleCurrentGroupColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        if group == GroupId::UNGROUPED {
            let msg = "Ungrouped keeps a fixed Neutral color; create or activate a custom group.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg), cx);
            cx.notify();
            return;
        }
        let Some(cur) = self.session_layout().group(group).map(|g| g.color) else {
            return;
        };
        let next = next_group_color(cur);
        let _ =
            self.run_ui_group_command(GroupCommand::SetColor { group, color: next }, window, cx);
    }

    pub(crate) fn on_toggle_current_group_collapse(
        &mut self,
        _: &ToggleCurrentGroupCollapse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        let Some(collapsed) = self.session_layout().group(group).map(|g| g.collapsed) else {
            return;
        };
        let _ = self.run_ui_group_command(
            GroupCommand::SetCollapsed {
                group,
                collapsed: !collapsed,
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_move_active_tab_earlier_in_group(
        &mut self,
        _: &MoveActiveTabEarlierInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        self.move_view_step_in_group(target, false, window, cx);
    }

    pub(crate) fn on_move_active_tab_later_in_group(
        &mut self,
        _: &MoveActiveTabLaterInGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        self.move_view_step_in_group(target, true, window, cx);
    }

    pub(crate) fn on_move_active_tab_to_next_group(
        &mut self,
        _: &MoveActiveTabToNextGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        let cur_group = self.session_layout().active_group();
        let Some(dest_group) = adjacent_group_id(self.session_layout(), cur_group, true) else {
            let msg = "Create another tab group first to move tabs across groups.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg), cx);
            cx.notify();
            return;
        };
        let _ = self.run_ui_group_command(
            GroupCommand::MoveView {
                target,
                group: dest_group,
                before: None,
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_move_active_tab_to_prev_group(
        &mut self,
        _: &MoveActiveTabToPrevGroup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        let cur_group = self.session_layout().active_group();
        let Some(dest_group) = adjacent_group_id(self.session_layout(), cur_group, false) else {
            let msg = "Create another tab group first to move tabs across groups.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg), cx);
            cx.notify();
            return;
        };
        let _ = self.run_ui_group_command(
            GroupCommand::MoveView {
                target,
                group: dest_group,
                before: None,
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_move_current_group_earlier(
        &mut self,
        _: &MoveCurrentGroupEarlier,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        self.move_group_step(group, false, window, cx);
    }

    pub(crate) fn on_move_current_group_later(
        &mut self,
        _: &MoveCurrentGroupLater,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        self.move_group_step(group, true, window, cx);
    }

    pub(crate) fn on_ungroup_active_tab(
        &mut self,
        _: &UngroupActiveTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.active_view_id().and_then(|id| self.view_target(id)) else {
            return;
        };
        if self.session_layout().active_group() == GroupId::UNGROUPED {
            self.status_flash = Some(("Tab is already in Ungrouped".to_string(), Instant::now()));
            cx.notify();
            return;
        }
        let _ = self.run_ui_group_command(
            GroupCommand::MoveView {
                target,
                group: GroupId::UNGROUPED,
                before: None,
            },
            window,
            cx,
        );
    }

    pub(crate) fn on_remove_current_group_keep_tabs(
        &mut self,
        _: &RemoveCurrentGroupKeepTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        if group == GroupId::UNGROUPED {
            let msg = "Ungrouped cannot be removed.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg), cx);
            cx.notify();
            return;
        }
        let _ = self.run_ui_group_command(GroupCommand::Remove { group }, window, cx);
    }

    pub(crate) fn on_close_current_group_tabs(
        &mut self,
        _: &CloseCurrentGroupTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self.session_layout().active_group();
        self.request_close_group(group, window, cx);
    }

    pub(crate) fn on_open_recovery_catalog(
        &mut self,
        _: &OpenRecoveryCatalog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_recovery_browser_open() {
            self.dismiss_recovery_browser(window, cx);
        } else {
            self.open_recovery_browser(None, window, cx);
        }
    }

    // --- Modal Overlay Renderers (In-Workspace, Zero Collision with Core Lifecycle Dialogs) ---

    pub(crate) fn render_group_editor_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.group_ui.editor_modal.as_ref()?;
        let is_dark = cx.theme().mode.is_dark();
        let raw_val = modal.name_input.read(cx).value().to_string();
        let trimmed_bytes = raw_val.trim().len();
        let over_limit = trimmed_bytes > MAX_GROUP_NAME_BYTES;
        let selected_color = modal.selected_color;

        let (title_text, submit_label, subtitle_text) = match modal.kind {
            GroupEditorModalKind::Create { initial_view, .. } => {
                let sub = if let Some(vt) = initial_view
                    && let Some(slot) = self.view_slot(vt.view_id)
                {
                    format!(
                        "Creates a custom tab group and moves \"{}\" into it.",
                        slot.title()
                    )
                } else {
                    "Creates an empty custom tab group in the tab bar.".to_string()
                };
                ("Create Tab Group", "Create Group", sub)
            }
            GroupEditorModalKind::Rename { group } => {
                let name = self
                    .session_layout()
                    .group(group)
                    .map(|g| g.name.as_str())
                    .unwrap_or("Group");
                (
                    "Rename & Style Tab Group",
                    "Save Group",
                    format!("Update name and accent color for \"{name}\"."),
                )
            }
        };

        let mut swatches = h_flex().gap_1p5().flex_wrap().items_center();
        for color in GroupColor::ALL {
            let is_selected = selected_color == color;
            let dot_color = group_color_hsla(color, is_dark);
            swatches = swatches.child(
                h_flex()
                    .id(("group-color-swatch", color as usize))
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .items_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .border_1()
                    .border_color(if is_selected {
                        cx.theme().primary
                    } else {
                        cx.theme().border
                    })
                    .bg(if is_selected {
                        cx.theme().list_active
                    } else {
                        cx.theme().secondary.opacity(0.35)
                    })
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .role(Role::RadioButton)
                    .aria_label(format!("Group color {}", color.label()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(m) = &mut this.group_ui.editor_modal {
                            m.selected_color = color;
                            let input = m.name_input.clone();
                            input.update(cx, |s, cx| s.focus(window, cx));
                        }
                        cx.notify();
                    }))
                    .child(div().size(px(10.)).rounded_full().bg(dot_color))
                    .child(div().text_xs().child(color.label())),
            );
        }

        let card = v_flex()
            .id("workspace-group-editor-card")
            .w(px(460.))
            .p_4()
            .gap_3()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .role(Role::Dialog)
            .aria_label(title_text)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().font_semibold().child(title_text))
                    .child(
                        Button::new("group-modal-close-x")
                            .accessibility_id("btn-group-modal-close")
                            .accessibility_label("Cancel Group Dialog")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Cancel (Esc)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_group_editor_modal(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(subtitle_text),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .text_xs()
                            .child(div().font_medium().child("Group Name"))
                            .child(
                                div()
                                    .text_color(if over_limit {
                                        cx.theme().danger
                                    } else {
                                        cx.theme().muted_foreground
                                    })
                                    .child(format!("{trimmed_bytes}/{MAX_GROUP_NAME_BYTES} bytes")),
                            ),
                    )
                    .child(
                        Input::new(&modal.name_input)
                            .small()
                            .cleanable(true)
                            .accessibility_id("input-group-name")
                            .aria_label("Tab group name"),
                    )
                    .when_some(modal.validation_error.clone(), |el, err_msg| {
                        el.child(
                            div()
                                .id("group-modal-validation-error")
                                .text_xs()
                                .text_color(cx.theme().danger)
                                .child(err_msg),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_1p5()
                    .child(div().text_xs().font_medium().child("Accent Color"))
                    .child(swatches),
            )
            .child(
                h_flex()
                    .w_full()
                    .pt_1()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("group-modal-cancel")
                            .accessibility_id("btn-group-modal-cancel")
                            .outline()
                            .small()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_group_editor_modal(window, cx);
                            })),
                    )
                    .child(
                        Button::new("group-modal-submit")
                            .accessibility_id("btn-group-modal-submit")
                            .primary()
                            .small()
                            .label(submit_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_group_editor_modal(window, cx);
                            })),
                    ),
            );

        Some(
            div()
                .id("workspace-group-editor-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(56.))
                .bg(black().opacity(0.28))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dismiss_group_editor_modal(window, cx);
                }))
                .child(card)
                .into_any_element(),
        )
    }

    pub(crate) fn render_recovery_browser_modal(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let modal = self.group_ui.recovery_modal.as_ref()?;
        let state = self.recovery_browser_state();
        let page = state.page.as_ref();
        let summary = page.map(|p| &p.summary).unwrap_or(&state.latest_summary);
        let page_num = modal.cursor_history.len() + 1;
        let has_prev = !modal.cursor_history.is_empty();
        let has_next = page.is_some_and(|p| p.next.is_some());
        let at_session_cap = self.session_layout().view_count() >= MAX_SESSION_VIEWS;
        let busy_loading_or_export = state.loading || state.active_export.is_some();

        let summary_strip = h_flex()
            .id("recovery-browser-summary")
            .w_full()
            .px_3()
            .py_2()
            .gap_3()
            .flex_wrap()
            .items_center()
            .rounded(px(6.))
            .bg(cx.theme().secondary.opacity(0.4))
            .border_1()
            .border_color(cx.theme().border)
            .text_xs()
            .child(
                div()
                    .font_semibold()
                    .child(format!("Total Eligible: {}", summary.total_eligible)),
            )
            .child(div().child(format!("Open (Admitted): {}", summary.admitted)))
            .child(div().child(format!(
                "Deferred Checkpoints: {}",
                summary.unplaced_checkpoints
            )))
            .child(div().child(format!("Deferred Other: {}", summary.unplaced_other)))
            .child(div().child(format!("Closed Drafts: {}", summary.closed)))
            .when(state.deferred_on_restore > 0, |el| {
                el.child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(cx.theme().warning.opacity(0.22))
                        .font_medium()
                        .child(format!(
                            "Deferred at Startup: {}",
                            state.deferred_on_restore
                        )),
                )
            })
            .when_some(
                page.map(|p| p.page_unreadable).filter(|&u| u > 0),
                |el, unread_count| {
                    el.child(
                        div()
                            .px_1p5()
                            .py_0p5()
                            .rounded(px(4.))
                            .bg(cx.theme().danger.opacity(0.18))
                            .text_color(cx.theme().danger)
                            .font_medium()
                            .child(format!(
                                "Unreadable on This Page: {unread_count} (page-local count)"
                            )),
                    )
                },
            );

        let capacity_callout = at_session_cap.then(|| {
            h_flex()
                .id("recovery-browser-capacity-notice")
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .rounded(px(5.))
                .bg(cx.theme().info.opacity(0.16))
                .border_1()
                .border_color(cx.theme().border)
                .text_xs()
                .child(
                    "Session is at the 1,024-tab limit. Use \"Export Copy…\" to save any stored draft directly to disk without opening a tab, or close an open tab (keeping its stored draft) before admitting another view.",
                )
        });

        let error_callout = state.error.as_ref().map(|err| {
            h_flex()
                .id("recovery-browser-error-banner")
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .justify_between()
                .rounded(px(5.))
                .bg(cx.theme().danger.opacity(0.15))
                .border_1()
                .border_color(cx.theme().danger.opacity(0.45))
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(cx.theme().danger)
                        .child(format!("Recovery catalog error: {}", err.message)),
                )
                .child(
                    Button::new("recovery-browser-retry-page")
                        .accessibility_id("btn-recovery-retry-page")
                        .outline()
                        .xsmall()
                        .label("Retry Page")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.retry_current_recovery_page(window, cx);
                        })),
                )
        });

        let rows_body: AnyElement = if let Some(p) = page {
            if p.entries.is_empty() {
                v_flex()
                    .flex_1()
                    .w_full()
                    .items_center()
                    .justify_center()
                    .p_6()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if state.loading {
                        "Loading recovery catalog page…"
                    } else {
                        "No recovery records on this page."
                    })
                    .into_any_element()
            } else {
                let mut list = div()
                    .id("recovery-browser-page-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .track_scroll(&modal.scroll_handle);

                for entry in &p.entries {
                    list =
                        list.child(self.render_recovery_entry_row(entry, state.active_export, cx));
                }
                list.into_any_element()
            }
        } else if state.loading {
            v_flex()
                .flex_1()
                .w_full()
                .items_center()
                .justify_center()
                .p_6()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Loading recovery catalog page…")
                .into_any_element()
        } else {
            v_flex()
                .flex_1()
                .w_full()
                .items_center()
                .justify_center()
                .p_6()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("No recovery catalog page loaded.")
                .child(
                    Button::new("recovery-browser-load-initial")
                        .primary()
                        .xsmall()
                        .label("Load Page 1")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.refresh_recovery_first_page(window, cx);
                        })),
                )
                .into_any_element()
        };

        let footer = h_flex()
            .w_full()
            .pt_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .items_center()
            .justify_between()
            .gap_2()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .child(
                        Button::new("recovery-page-first")
                            .accessibility_id("btn-recovery-first-page")
                            .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .label("Refresh / Page 1")
                            .disabled(state.loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh_recovery_first_page(window, cx);
                            })),
                    )
                    .child(
                        Button::new("recovery-page-prev")
                            .accessibility_id("btn-recovery-prev-page")
                            .outline()
                            .xsmall()
                            .icon(IconName::ChevronLeft)
                            .label("Previous Page")
                            .disabled(!has_prev || state.loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_recovery_prev_page(window, cx);
                            })),
                    )
                    .child(
                        Button::new("recovery-page-next")
                            .accessibility_id("btn-recovery-next-page")
                            .outline()
                            .xsmall()
                            .label("Next Page")
                            .icon(IconName::ChevronRight)
                            .disabled(!has_next || state.loading)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_recovery_next_page(window, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "Page {page_num}{}",
                                if state.loading { " · Loading…" } else { "" }
                            )),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(busy_loading_or_export, |el| {
                        el.child(
                            Button::new("recovery-cancel-request")
                                .accessibility_id("btn-recovery-cancel-request")
                                .danger()
                                .xsmall()
                                .label(if state.active_export.is_some() {
                                    "Cancel Pending Hydrate"
                                } else {
                                    "Cancel Request"
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_recovery_request(cx);
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new("recovery-browser-close")
                            .accessibility_id("btn-recovery-browser-close")
                            .outline()
                            .small()
                            .label("Close")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_recovery_browser(window, cx);
                            })),
                    ),
            );

        let card = v_flex()
            .id("workspace-recovery-browser-card")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&modal.focus_handle)
            .w(px(780.))
            .h(px(540.))
            .p_4()
            .gap_2p5()
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_lg()
            .overflow_hidden()
            .role(Role::Dialog)
            .aria_label("Recovery Draft Catalog")
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                this.dismiss_recovery_browser(window, cx);
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child("Recovery Draft Catalog & Overflow Browser"),
                    )
                    .child(
                        Button::new("recovery-browser-close-x")
                            .accessibility_id("btn-recovery-browser-close-x")
                            .accessibility_label("Close Recovery Catalog")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip("Close (Esc)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.dismiss_recovery_browser(window, cx);
                            })),
                    ),
            )
            .child(summary_strip)
            .when_some(capacity_callout, |el, banner| el.child(banner))
            .when_some(error_callout, |el, banner| el.child(banner))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .child(rows_body),
            )
            .child(footer);

        Some(
            div()
                .id("workspace-recovery-browser-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .justify_center()
                .items_start()
                .pt(px(36.))
                .bg(black().opacity(0.30))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.dismiss_recovery_browser(window, cx);
                }))
                .child(card)
                .into_any_element(),
        )
    }

    fn render_recovery_entry_row(
        &self,
        entry: &RecoveryEntry,
        active_export: Option<RecoveryEntryKey>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = entry.key;
        let origin_slug = recovery_origin_slug(key.origin);
        let is_exporting_this = active_export == Some(key);
        let export_busy_other = active_export.is_some() && !is_exporting_this;
        let (cat_label, cat_bg) = match entry.category {
            RecoveryEntryCategory::Admitted => ("OPEN", cx.theme().primary.opacity(0.18)),
            RecoveryEntryCategory::Closed => ("CLOSED", cx.theme().secondary),
            RecoveryEntryCategory::UnplacedCheckpoint => {
                ("DEFERRED DRAFT", cx.theme().warning.opacity(0.22))
            }
            RecoveryEntryCategory::UnplacedOther => {
                ("DEFERRED META", cx.theme().info.opacity(0.18))
            }
        };

        match &entry.stub {
            Ok(stub) => {
                let title = if stub.meta.title.is_empty() {
                    "untitled".to_string()
                } else {
                    stub.meta.title.clone()
                };
                let path_detail = stub
                    .meta
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "Unsaved / Untitled record".to_string());
                let saved_size = stub
                    .meta
                    .saved_identity
                    .map(|id| format!(" · disk {}", format_bytes(id.byte_len)))
                    .unwrap_or_default();
                let placement_note = entry
                    .last_placement
                    .as_ref()
                    .map(|closed| {
                        let grp_name = self
                            .session_layout()
                            .group(closed.previous_view.group_id)
                            .map(|g| g.name.as_str())
                            .unwrap_or(if closed.previous_view.group_id == GroupId::UNGROUPED {
                                "Ungrouped"
                            } else {
                                "Removed Group"
                            });
                        format!(" · Last in \"{grp_name}\" #{}", closed.position + 1)
                    })
                    .unwrap_or_default();

                let detail_line = format!(
                    "{path_detail} · {origin_slug} row #{} · rev {} · {}{saved_size}{placement_note}",
                    key.rowid,
                    stub.durable_rev.0,
                    if entry.has_checkpoint {
                        "Checkpoint stored"
                    } else {
                        "Metadata record"
                    }
                );

                let is_live_admitted = self
                    .session_layout()
                    .views
                    .values()
                    .any(|v| v.document == stub.key());
                let open_label = if is_live_admitted { "Activate" } else { "Open" };
                h_flex()
                    .id(recovery_elem_id(*b"rcvr", key))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2p5()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(3.))
                                            .bg(cat_bg)
                                            .text_xs()
                                            .font_medium()
                                            .flex_shrink_0()
                                            .child(cat_label),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_semibold()
                                            .truncate()
                                            .child(title),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .truncate()
                                    .child(detail_line),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_shrink_0()
                            .items_center()
                            .child(
                                Button::new(recovery_elem_id(*b"rcvo", key))
                                    .accessibility_id(format!(
                                        "btn-recovery-open-{origin_slug}-{}",
                                        key.rowid
                                    ))
                                    .outline()
                                    .xsmall()
                                    .label(open_label)
                                    .tooltip(if is_live_admitted {
                                        "Activate already-open workspace view"
                                    } else {
                                        "Admit and open this recovery record in the workspace"
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_recovery_entry(key, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(recovery_elem_id(*b"rcve", key))
                                    .accessibility_id(format!(
                                        "btn-recovery-export-{origin_slug}-{}",
                                        key.rowid
                                    ))
                                    .primary()
                                    .xsmall()
                                    .label(if is_exporting_this {
                                        "Exporting…"
                                    } else {
                                        "Export Copy…"
                                    })
                                    .disabled(is_exporting_this || export_busy_other)
                                    .tooltip(if entry.has_checkpoint {
                                        "Export stored draft copy to disk without creating an EditorState or altering source association"
                                    } else {
                                        "Export copy to disk without creating an EditorState or altering source association"
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.export_recovery_entry(key, window, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            Err(err) => {
                let doc_hex = key
                    .doc_id
                    .map(|d| format!("{:032x}", d.0))
                    .unwrap_or_else(|| "unknown-doc".to_string());
                let gen_txt = key
                    .lifecycle_gen
                    .map(|g| format!("gen {}", g.0))
                    .unwrap_or_else(|| "gen —".to_string());
                let record_header = match key.origin {
                    RecoveryEntryOrigin::Document => {
                        format!(
                            "Preserved document row #{} · {doc_hex} · {gen_txt}",
                            key.rowid
                        )
                    }
                    RecoveryEntryOrigin::LegacyOrphan => format!(
                        "Preserved legacy v1 view row #{} · {doc_hex} · {gen_txt}",
                        key.rowid
                    ),
                };
                let head_note = if entry.has_checkpoint {
                    " (matching recovery head noted)"
                } else {
                    ""
                };

                h_flex()
                    .id(recovery_elem_id(*b"rcvr", key))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2p5()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().danger.opacity(0.06))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(3.))
                                            .bg(cx.theme().danger.opacity(0.2))
                                            .text_color(cx.theme().danger)
                                            .text_xs()
                                            .font_semibold()
                                            .flex_shrink_0()
                                            .child("INACCESSIBLE RECORD"),
                                    )
                                    .child(
                                        div()
                                            .px_1p5()
                                            .py_0p5()
                                            .rounded(px(3.))
                                            .bg(cat_bg)
                                            .text_xs()
                                            .flex_shrink_0()
                                            .child(cat_label),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_medium()
                                            .truncate()
                                            .child(record_header),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .child(format!(
                                        "Unreadable metadata/record ({}){head_note} — raw SQLite record preserved; not salvageable via editor.",
                                        err.message
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .flex_shrink_0()
                            .child("Preserved raw"),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Lazily builds the right-click context menu for a tab or sidebar Open Editor row.
/// Captures only `(Entity<EditorWorkspace>, ViewTarget)` on render — zero per-tab cloning of
/// groups or member lists across 1,024 tabs.
pub(crate) fn build_tab_context_menu(
    menu: PopupMenu,
    workspace: Entity<EditorWorkspace>,
    target: ViewTarget,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let is_dark = cx.theme().mode.is_dark();
    let (
        pre_menu_origin,
        resolved_pane_id,
        resolved_group_id,
        resolved_group_name,
        resolved_group_collapsed,
        can_move_earlier,
        can_move_later,
        has_disk_path,
        can_save_in_place,
        can_save_as,
        has_checkpoint_or_overbudget,
        groups_snapshot,
        other_panes_snapshot,
        can_split_h,
        can_split_v,
        lifecycle_busy,
    ) = {
        let ws = workspace.read(cx);
        let origin = ws.capture_overlay_origin(window, cx);
        let layout = ws.session_layout();
        let Ok(lv) = layout.resolve(target) else {
            return menu.min_w(px(200.)).label("Tab is no longer open");
        };
        let Some(owning_pane) = layout.pane_for_view(target.view_id) else {
            return menu.min_w(px(200.)).label("Tab is no longer placed");
        };
        let grp_id = lv.group_id;
        let (grp_name, grp_collapsed) = layout
            .group(grp_id)
            .map(|g| (g.name.clone(), g.collapsed))
            .unwrap_or_else(|| ("Ungrouped".to_string(), false));
        let earlier = prev_view_anchor_in_group(layout, target).is_some();
        let later = next_view_anchor_in_group(layout, target).is_some();
        let slot = ws.view_slot(target.view_id);
        let disk_path = slot.is_some_and(|s| s.meta().path.is_some());
        let save_ok = matches!(slot, Some(TabSlot::Editable(ed)) if !ed.is_readonly());
        let save_as_ok = matches!(slot, Some(TabSlot::Editable(_)));
        let rescue_ok = slot.is_some_and(|s| {
            s.has_checkpoint()
                || matches!(s, TabSlot::OverBudgetDraft { .. } | TabSlot::Failed { .. })
        });
        let groups_vec: Vec<(GroupId, String, GroupColor)> = layout
            .tree
            .groups
            .iter()
            .map(|g| (g.id, g.name.clone(), g.color))
            .collect();
        let mut total_panes = 0usize;
        let mut other_panes = Vec::new();
        for (ix, p) in layout.panes().enumerate() {
            total_panes += 1;
            if p.id != owning_pane {
                other_panes.push((ix + 1, p.id, p.tabs.len()));
            }
        }
        let root_bounds = ws.group_ui.panes.root_bounds();
        let split_h_ok = total_panes < MAX_PANES
            && pane_ui::can_split_pane_in_bounds(
                &layout.tree.root,
                root_bounds,
                owning_pane,
                SplitDirection::Right,
                &|id, fb| ws.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();
        let split_v_ok = total_panes < MAX_PANES
            && pane_ui::can_split_pane_in_bounds(
                &layout.tree.root,
                root_bounds,
                owning_pane,
                SplitDirection::Down,
                &|id, fb| ws.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();
        (
            origin,
            owning_pane,
            grp_id,
            grp_name,
            grp_collapsed,
            earlier,
            later,
            disk_path,
            save_ok,
            save_as_ok,
            rescue_ok,
            groups_vec,
            other_panes,
            split_h_ok,
            split_v_ok,
            ws.is_lifecycle_busy(),
        )
    };

    let ws_new_grp = workspace.clone();
    let ws_ungrp = workspace.clone();
    let ws_earlier = workspace.clone();
    let ws_later = workspace.clone();
    let ws_save = workspace.clone();
    let ws_save_as = workspace.clone();
    let ws_reload = workspace.clone();
    let ws_export = workspace.clone();
    let ws_close = workspace.clone();
    let ws_submenu = workspace.clone();

    let mut m = menu
        .min_w(px(235.))
        .item(
            PopupMenuItem::new("New Group with This Tab…")
                .disabled(lifecycle_busy)
                .on_click(move |_, window, cx| {
                    ws_new_grp.update(cx, |ws, cx| {
                        ws.open_create_group_modal(
                            Some(target),
                            None,
                            None,
                            Some(pre_menu_origin),
                            window,
                            cx,
                        );
                    });
                }),
        )
        .submenu(
            "Move Tab to Group",
            window,
            cx,
            move |mut sub, _window, _cx| {
                sub = sub.min_w(px(210.)).scrollable(true).max_h(px(300.));
                for (gid, gname, gcolor) in &groups_snapshot {
                    let gid_copy = *gid;
                    let gname_copy = gname.clone();
                    let dot_hsla = group_color_hsla(*gcolor, is_dark);
                    let is_current = gid_copy == resolved_group_id;
                    let ws_mv = ws_submenu.clone();
                    sub = sub.item(
                        PopupMenuItem::element(move |_w, _cx| {
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().size(px(8.)).rounded_full().bg(dot_hsla))
                                .child(div().truncate().max_w(px(165.)).child(gname_copy.clone()))
                        })
                        .checked(is_current)
                        .disabled(lifecycle_busy)
                        .on_click(move |_, window, cx| {
                            ws_mv.update(cx, |ws, cx| {
                                let _ = ws.run_ui_group_command(
                                    GroupCommand::MoveView {
                                        target,
                                        group: gid_copy,
                                        before: None,
                                    },
                                    window,
                                    cx,
                                );
                            });
                        }),
                    );
                }
                sub
            },
        )
        .item(
            PopupMenuItem::new("Move to Ungrouped")
                .disabled(lifecycle_busy || resolved_group_id == GroupId::UNGROUPED)
                .on_click(move |_, window, cx| {
                    ws_ungrp.update(cx, |ws, cx| {
                        let _ = ws.run_ui_group_command(
                            GroupCommand::MoveView {
                                target,
                                group: GroupId::UNGROUPED,
                                before: None,
                            },
                            window,
                            cx,
                        );
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Move Tab Earlier in Group")
                .disabled(lifecycle_busy || !can_move_earlier)
                .on_click(move |_, window, cx| {
                    ws_earlier.update(cx, |ws, cx| {
                        ws.move_view_step_in_group(target, false, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Move Tab Later in Group")
                .disabled(lifecycle_busy || !can_move_later)
                .on_click(move |_, window, cx| {
                    ws_later.update(cx, |ws, cx| {
                        ws.move_view_step_in_group(target, true, window, cx);
                    });
                }),
        )
        .separator();
    {
        let ws_sp_r = workspace.clone();
        let ws_sp_d = workspace.clone();
        let ws_sp_l = workspace.clone();
        let ws_sp_u = workspace.clone();
        m = m
            .item(
                PopupMenuItem::new("Split & Move Tab Right")
                    .disabled(lifecycle_busy || !can_split_h)
                    .on_click(move |_, window, cx| {
                        ws_sp_r.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                resolved_pane_id,
                                SplitDirection::Right,
                                Some(PaneMove {
                                    target,
                                    source: resolved_pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Tab Down")
                    .disabled(lifecycle_busy || !can_split_v)
                    .on_click(move |_, window, cx| {
                        ws_sp_d.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                resolved_pane_id,
                                SplitDirection::Down,
                                Some(PaneMove {
                                    target,
                                    source: resolved_pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Tab Left")
                    .disabled(lifecycle_busy || !can_split_h)
                    .on_click(move |_, window, cx| {
                        ws_sp_l.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                resolved_pane_id,
                                SplitDirection::Left,
                                Some(PaneMove {
                                    target,
                                    source: resolved_pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Tab Up")
                    .disabled(lifecycle_busy || !can_split_v)
                    .on_click(move |_, window, cx| {
                        ws_sp_u.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                resolved_pane_id,
                                SplitDirection::Up,
                                Some(PaneMove {
                                    target,
                                    source: resolved_pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            );

        if !other_panes_snapshot.is_empty() {
            let ws_mv_pane = workspace.clone();
            m = m.submenu("Move Tab to Pane", window, cx, move |mut sub, _w, _cx| {
                sub = sub.min_w(px(195.));
                for &(pnum, pid, ptabs) in &other_panes_snapshot {
                    let ws_dest = ws_mv_pane.clone();
                    sub = sub.item(
                        PopupMenuItem::element(move |_w, cx| {
                            h_flex()
                                .gap_2()
                                .items_center()
                                .justify_between()
                                .w_full()
                                .child(format!("Pane {pnum}"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!("{ptabs} tabs")),
                                )
                        })
                        .disabled(lifecycle_busy)
                        .on_click(move |_, window, cx| {
                            ws_dest.update(cx, |ws, cx| {
                                let _ = ws.move_view_to_pane(
                                    target,
                                    resolved_pane_id,
                                    pid,
                                    None,
                                    None,
                                    window,
                                    cx,
                                );
                            });
                        }),
                    );
                }
                sub
            });
        }
        m = m.separator();
    }

    if resolved_group_id != GroupId::UNGROUPED {
        let ws_ren = workspace.clone();
        let ws_col = workspace.clone();
        let ws_rem = workspace.clone();
        let ws_cls_grp = workspace;
        m = m
            .item(
                PopupMenuItem::new(format!("Rename Group \"{resolved_group_name}\"…"))
                    .disabled(lifecycle_busy)
                    .on_click(move |_, window, cx| {
                        ws_ren.update(cx, |ws, cx| {
                            ws.open_rename_group_modal(
                                resolved_group_id,
                                Some(pre_menu_origin),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(if resolved_group_collapsed {
                    format!("Expand Group \"{resolved_group_name}\"")
                } else {
                    format!("Collapse Group \"{resolved_group_name}\"")
                })
                .disabled(lifecycle_busy)
                .on_click(move |_, window, cx| {
                    ws_col.update(cx, |ws, cx| {
                        let _ = ws.run_ui_group_command(
                            GroupCommand::SetCollapsed {
                                group: resolved_group_id,
                                collapsed: !resolved_group_collapsed,
                            },
                            window,
                            cx,
                        );
                    });
                }),
            )
            .item(
                PopupMenuItem::new("Remove Group (Keep Tabs in Ungrouped)")
                    .disabled(lifecycle_busy)
                    .on_click(move |_, window, cx| {
                        ws_rem.update(cx, |ws, cx| {
                            let _ = ws.run_ui_group_command(
                                GroupCommand::Remove {
                                    group: resolved_group_id,
                                },
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new(format!("Close All Tabs in \"{resolved_group_name}\"…"))
                    .disabled(lifecycle_busy)
                    .on_click(move |_, window, cx| {
                        ws_cls_grp.update(cx, |ws, cx| {
                            ws.request_close_group(resolved_group_id, window, cx);
                        });
                    }),
            )
            .separator();
    }

    m = m
        .item(
            PopupMenuItem::new("Save")
                .disabled(lifecycle_busy || !can_save_in_place)
                .on_click(move |_, window, cx| {
                    ws_save.update(cx, |ws, cx| {
                        ws.save_view(target, None, false, false, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Save As…")
                .disabled(lifecycle_busy || !can_save_as)
                .on_click(move |_, window, cx| {
                    ws_save_as.update(cx, |ws, cx| {
                        ws.prompt_save_view_as(target, false, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Reload from Disk")
                .disabled(lifecycle_busy || !has_disk_path)
                .on_click(move |_, window, cx| {
                    ws_reload.update(cx, |ws, cx| {
                        ws.request_reload_view(target, window, cx);
                    });
                }),
        );

    if has_checkpoint_or_overbudget {
        m = m.item(PopupMenuItem::new("Export Stored Draft Copy…").on_click(
            move |_, window, cx| {
                ws_export.update(cx, |ws, cx| {
                    ws.export_view_draft_copy(target, window, cx);
                });
            },
        ));
    }

    m.separator().item(
        PopupMenuItem::new("Close Tab")
            .disabled(lifecycle_busy)
            .on_click(move |_, window, cx| {
                ws_close.update(cx, |ws, cx| {
                    ws.request_close_view(target, window, cx);
                });
            }),
    )
}

/// Lazily builds the dropdown / context menu for a `TabGroup` header.
/// Clearly separates `Remove Group (Keep Tabs Open in Ungrouped)` from `Close All Tabs in Group…`.
pub(crate) fn build_group_menu(
    menu: PopupMenu,
    workspace: Entity<EditorWorkspace>,
    pane_id: PaneId,
    group_id: GroupId,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let is_dark = cx.theme().mode.is_dark();
    let (
        pre_menu_origin,
        group_name,
        group_color,
        is_collapsed,
        pane_member_count,
        global_member_count,
        can_move_earlier,
        can_move_later,
        lifecycle_busy,
    ) = {
        let ws = workspace.read(cx);
        let origin = ws.capture_overlay_origin(window, cx);
        let layout = ws.session_layout();
        let Some(grp) = layout.group(group_id) else {
            return menu.min_w(px(200.)).label("Group no longer exists");
        };
        let pane_count = layout
            .pane(pane_id)
            .map(|p| {
                p.tabs
                    .iter()
                    .filter(|id| layout.view(**id).is_some_and(|v| v.group_id == group_id))
                    .count()
            })
            .unwrap_or(0);
        let global_count = layout
            .view_ids()
            .filter(|id| layout.view(*id).is_some_and(|v| v.group_id == group_id))
            .count();
        (
            origin,
            grp.name.clone(),
            grp.color,
            grp.collapsed,
            pane_count,
            global_count,
            prev_group_anchor(layout, group_id).is_some(),
            next_group_anchor(layout, group_id).is_some(),
            ws.is_lifecycle_busy(),
        )
    };

    let is_custom = group_id != GroupId::UNGROUPED;
    let ws_toggle = workspace.clone();
    let ws_new_tab = workspace.clone();
    let ws_create_after = workspace.clone();
    let ws_earlier = workspace.clone();
    let ws_later = workspace.clone();
    let ws_close_all = workspace.clone();

    let header_summary = if pane_member_count == global_member_count {
        format!("{group_name} ({pane_member_count})")
    } else {
        format!("{group_name} ({pane_member_count} in pane · {global_member_count} total)")
    };
    let mut m = menu
        .min_w(px(250.))
        .label(header_summary)
        .item(
            PopupMenuItem::new(if is_collapsed {
                "Expand Group Tabs"
            } else {
                "Collapse Group Tabs"
            })
            .disabled(lifecycle_busy)
            .on_click(move |_, window, cx| {
                ws_toggle.update(cx, |ws, cx| {
                    let _ = ws.run_ui_group_command(
                        GroupCommand::SetCollapsed {
                            group: group_id,
                            collapsed: !is_collapsed,
                        },
                        window,
                        cx,
                    );
                });
            }),
        )
        .item(
            PopupMenuItem::new("New Untitled Tab in Group")
                .disabled(lifecycle_busy)
                .on_click(move |_, window, cx| {
                    ws_new_tab.update(cx, |ws, cx| {
                        ws.create_new_tab_in_group(pane_id, group_id, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Create New Group…")
                .disabled(lifecycle_busy)
                .on_click(move |_, window, cx| {
                    ws_create_after.update(cx, |ws, cx| {
                        ws.open_create_group_modal(
                            None,
                            None,
                            None,
                            Some(pre_menu_origin),
                            window,
                            cx,
                        );
                    });
                }),
        );

    if is_custom {
        let ws_rename = workspace.clone();
        let ws_colors = workspace.clone();
        m = m
            .separator()
            .item(
                PopupMenuItem::new("Rename Group…")
                    .disabled(lifecycle_busy)
                    .on_click(move |_, window, cx| {
                        ws_rename.update(cx, |ws, cx| {
                            ws.open_rename_group_modal(group_id, Some(pre_menu_origin), window, cx);
                        });
                    }),
            )
            .submenu("Group Color", window, cx, move |mut sub, _window, _cx| {
                sub = sub.min_w(px(175.));
                for color in GroupColor::ALL {
                    let dot = group_color_hsla(color, is_dark);
                    let ws_c = ws_colors.clone();
                    sub = sub.item(
                        PopupMenuItem::element(move |_w, _cx| {
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().size(px(9.)).rounded_full().bg(dot))
                                .child(color.label())
                        })
                        .checked(group_color == color)
                        .disabled(lifecycle_busy)
                        .on_click(move |_, window, cx| {
                            ws_c.update(cx, |ws, cx| {
                                let _ = ws.run_ui_group_command(
                                    GroupCommand::SetColor {
                                        group: group_id,
                                        color,
                                    },
                                    window,
                                    cx,
                                );
                            });
                        }),
                    );
                }
                sub
            });
    }

    m = m
        .separator()
        .item(
            PopupMenuItem::new("Move Group Earlier")
                .disabled(lifecycle_busy || !can_move_earlier)
                .on_click(move |_, window, cx| {
                    ws_earlier.update(cx, |ws, cx| {
                        ws.move_group_step(group_id, false, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Move Group Later")
                .disabled(lifecycle_busy || !can_move_later)
                .on_click(move |_, window, cx| {
                    ws_later.update(cx, |ws, cx| {
                        ws.move_group_step(group_id, true, window, cx);
                    });
                }),
        )
        .separator();

    if is_custom {
        let ws_remove = workspace;
        m = m.item(
            PopupMenuItem::new("Remove Group (Keep Tabs Open in Ungrouped)")
                .disabled(lifecycle_busy)
                .on_click(move |_, window, cx| {
                    ws_remove.update(cx, |ws, cx| {
                        let _ = ws.run_ui_group_command(
                            GroupCommand::Remove { group: group_id },
                            window,
                            cx,
                        );
                    });
                }),
        );
    }

    m.item(
        PopupMenuItem::new("Close All Tabs in Group…")
            .disabled(lifecycle_busy || global_member_count == 0)
            .on_click(move |_, window, cx| {
                ws_close_all.update(cx, |ws, cx| {
                    ws.request_close_group(group_id, window, cx);
                });
            }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_services::{DocumentId, DocumentKey, LayoutView, LifecycleGen, ViewState};

    fn insert_test_view(layout: &mut SessionLayout, group: GroupId) -> ViewTarget {
        let id = ViewId::new();
        let document = DocumentKey {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
        };
        let pane_id = layout.active_pane_id();
        layout
            .insert_view(
                pane_id,
                LayoutView {
                    id,
                    document,
                    group_id: group,
                    view: ViewState::default(),
                },
                None,
                true,
            )
            .unwrap();
        ViewTarget {
            view_id: id,
            document,
        }
    }

    #[test]
    fn group_name_draft_validation_enforces_trim_control_chars_and_128_utf8_bytes() {
        assert!(validate_group_name_draft("   ").is_err());
        assert!(validate_group_name_draft("bad\nname").is_err());
        assert!(validate_group_name_draft("bad\tname").is_err());
        let exact_128 = "é".repeat(64);
        assert_eq!(exact_128.len(), 128);
        assert_eq!(
            validate_group_name_draft(&format!("  {exact_128}  ")).unwrap(),
            (exact_128.as_str(), 128)
        );
        let over_128 = format!("{exact_128}a");
        assert!(validate_group_name_draft(&over_128).is_err());
    }

    #[test]
    fn step_reorder_anchors_move_tabs_and_groups_by_exact_single_slot() {
        let mut layout = SessionLayout::default();
        let g_alpha = layout
            .apply_group_command(GroupCommand::Create {
                name: "Alpha".into(),
                color: GroupColor::Red,
                before: None,
                initial_view: None,
            })
            .unwrap()
            .created_group
            .unwrap();
        let g_beta = layout
            .apply_group_command(GroupCommand::Create {
                name: "Beta".into(),
                color: GroupColor::Teal,
                before: None,
                initial_view: None,
            })
            .unwrap()
            .created_group
            .unwrap();

        let v1 = insert_test_view(&mut layout, g_alpha);
        let v2 = insert_test_view(&mut layout, g_alpha);
        let v3 = insert_test_view(&mut layout, g_alpha);
        assert_eq!(
            layout.active_pane().tabs,
            vec![v1.view_id, v2.view_id, v3.view_id]
        );

        // v1 is first in Alpha: earlier is None, later swaps past v2.
        assert_eq!(prev_view_anchor_in_group(&layout, v1), None);
        let (grp, before) = next_view_anchor_in_group(&layout, v1).unwrap();
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: v1,
                group: grp,
                before,
            })
            .unwrap();
        assert_eq!(
            layout.active_pane().tabs,
            vec![v2.view_id, v1.view_id, v3.view_id]
        );

        // Move v1 one more slot later -> reaches end of Alpha.
        let (grp, before) = next_view_anchor_in_group(&layout, v1).unwrap();
        assert_eq!(before, None);
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: v1,
                group: grp,
                before,
            })
            .unwrap();
        assert_eq!(
            layout.active_pane().tabs,
            vec![v2.view_id, v3.view_id, v1.view_id]
        );
        assert_eq!(next_view_anchor_in_group(&layout, v1), None);

        // Move v1 one slot earlier -> back between v2 and v3.
        let (grp, before) = prev_view_anchor_in_group(&layout, v1).unwrap();
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: v1,
                group: grp,
                before,
            })
            .unwrap();
        assert_eq!(
            layout.active_pane().tabs,
            vec![v2.view_id, v1.view_id, v3.view_id]
        );

        // Group order starts [Ungrouped, Alpha, Beta]. Move Beta earlier -> [Ungrouped, Beta, Alpha].
        let before_g = prev_group_anchor(&layout, g_beta).unwrap();
        layout
            .apply_group_command(GroupCommand::Reorder {
                group: g_beta,
                before: before_g,
            })
            .unwrap();
        assert_eq!(
            layout.tree.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![GroupId::UNGROUPED, g_beta, g_alpha]
        );
        // Move Ungrouped later -> [Beta, Ungrouped, Alpha].
        let after_u = next_group_anchor(&layout, GroupId::UNGROUPED).unwrap();
        layout
            .apply_group_command(GroupCommand::Reorder {
                group: GroupId::UNGROUPED,
                before: after_u,
            })
            .unwrap();
        assert_eq!(
            layout.tree.groups.iter().map(|g| g.id).collect::<Vec<_>>(),
            vec![g_beta, GroupId::UNGROUPED, g_alpha]
        );
        assert_eq!(adjacent_group_id(&layout, g_alpha, true), Some(g_beta));
        assert_eq!(adjacent_group_id(&layout, g_beta, false), Some(g_alpha));
        layout.validate().unwrap();
    }

    #[test]
    fn opaque_element_ids_preserve_full_128_bit_and_origin_rowid_identities() {
        let a = ViewId(1);
        let b = ViewId((1u128 << 64) | 1);
        assert_ne!(view_elem_id(*b"tabi", a), view_elem_id(*b"tabi", b));
        assert_ne!(view_elem_id(*b"tabi", a), view_elem_id(*b"tabx", a));

        for rowid in [i64::MIN, -1, 0, 1, 42, i64::MAX] {
            let doc_key = RecoveryEntryKey {
                origin: RecoveryEntryOrigin::Document,
                rowid,
                doc_id: None,
                lifecycle_gen: Some(LifecycleGen(1)),
            };
            let orphan_key = RecoveryEntryKey {
                origin: RecoveryEntryOrigin::LegacyOrphan,
                rowid,
                doc_id: None,
                lifecycle_gen: None,
            };
            assert_ne!(
                recovery_elem_id(*b"rcvr", doc_key),
                recovery_elem_id(*b"rcvr", orphan_key)
            );
            assert_ne!(
                recovery_elem_id(*b"rcvo", doc_key),
                recovery_elem_id(*b"rcve", doc_key)
            );
        }
        let min_doc = RecoveryEntryKey {
            origin: RecoveryEntryOrigin::Document,
            rowid: i64::MIN,
            doc_id: None,
            lifecycle_gen: None,
        };
        let zero_doc = RecoveryEntryKey {
            origin: RecoveryEntryOrigin::Document,
            rowid: 0,
            doc_id: None,
            lifecycle_gen: None,
        };
        assert_ne!(
            recovery_elem_id(*b"rcvr", min_doc),
            recovery_elem_id(*b"rcvr", zero_doc)
        );
    }
}
