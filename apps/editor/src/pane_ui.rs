use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use editor_services::{
    GroupId, LayoutRoot, MAX_PANES, MAX_SPLIT_RATIO_BPS, MIN_SPLIT_RATIO_BPS, PaneCommand,
    PaneCommandResult, PaneId, PaneMove, SessionLayout, SplitAxis, SplitDirection, SplitNodeId,
    ViewTarget,
};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex};
use gpui_kit::{
    Anchor, Bounds, Context, ElementId, Entity, ParentElement as _, Pixels, Point, ScrollHandle,
    Styled as _, Window, div, point, px, size,
};

use crate::actions::{
    CloseActivePane, CloseActivePaneTabs, EqualizePanes, FocusNextPane, FocusPrevPane,
    MoveActiveTabToNextPane, MoveActiveTabToPrevPane, SplitAndMoveActiveTabDown,
    SplitAndMoveActiveTabLeft, SplitAndMoveActiveTabRight, SplitAndMoveActiveTabUp, SplitPaneDown,
    SplitPaneLeft, SplitPaneRight, SplitPaneUp, ToggleFocusActivePane,
};
use crate::app::{EditorWorkspace, OverlayOrigin};
use crate::group_ui::DraggedTabView;

pub(crate) const MIN_PANE_WIDTH_PX: f32 = 240.0;
pub(crate) const MIN_PANE_HEIGHT_PX: f32 = 160.0;
pub(crate) const SPLIT_DIVIDER_THICKNESS_PX: f32 = 5.0;
pub(crate) const MAX_SPLIT_NODES: usize = MAX_PANES - 1;

/// UI-owned Command Palette action payload for Split Panes, nested inside `GroupPaletteAction::Pane`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanePaletteAction {
    Split {
        pane: PaneId,
        direction: SplitDirection,
        moving: Option<(PaneId, ViewTarget)>,
    },
    FocusNextPane,
    FocusPrevPane,
    ActivatePane {
        pane: PaneId,
    },
    MoveTabToPane {
        target: ViewTarget,
        source: PaneId,
        destination: PaneId,
    },
    MoveTabAdjacentPane {
        target: ViewTarget,
        source: PaneId,
        forward: bool,
    },
    Equalize,
    SetFocusOnly {
        enabled: bool,
    },
    ClosePaneMerge {
        pane: PaneId,
    },
    ClosePaneTabs {
        pane: PaneId,
    },
}

/// Builds a stack-only, collision-free 20-byte `ElementId` from a 4-byte tag and a 128-bit `PaneId`.
#[inline]
pub(crate) fn pane_elem_id(tag: [u8; 4], id: PaneId) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4..].copy_from_slice(&id.0.to_be_bytes());
    ElementId::from(bytes)
}

/// Builds a stack-only, collision-free 20-byte `ElementId` from a 4-byte tag and a 128-bit `SplitNodeId`.
#[inline]
pub(crate) fn split_elem_id(tag: [u8; 4], id: SplitNodeId) -> ElementId {
    let mut bytes = [0u8; 20];
    bytes[..4].copy_from_slice(&tag);
    bytes[4..].copy_from_slice(&id.0.to_be_bytes());
    ElementId::from(bytes)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SplitGeometry {
    pub(crate) node_id: SplitNodeId,
    pub(crate) axis: SplitAxis,
    pub(crate) ratio_bps: u16,
    pub(crate) split_bounds: Bounds<Pixels>,
    pub(crate) divider_bounds: Bounds<Pixels>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TreeGeometry {
    leaves: [(PaneId, Bounds<Pixels>); MAX_PANES],
    leaf_len: usize,
    splits: [SplitGeometry; MAX_SPLIT_NODES],
    split_len: usize,
}

impl Default for TreeGeometry {
    fn default() -> Self {
        let zero_bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0)));
        let empty_split = SplitGeometry {
            node_id: SplitNodeId(0),
            axis: SplitAxis::Horizontal,
            ratio_bps: 5_000,
            split_bounds: zero_bounds,
            divider_bounds: zero_bounds,
        };
        Self {
            leaves: [(PaneId::default(), zero_bounds); MAX_PANES],
            leaf_len: 0,
            splits: [empty_split; MAX_SPLIT_NODES],
            split_len: 0,
        }
    }
}

impl TreeGeometry {
    #[inline]
    pub(crate) fn leaves(&self) -> &[(PaneId, Bounds<Pixels>)] {
        &self.leaves[..self.leaf_len]
    }

    #[inline]
    pub(crate) fn splits(&self) -> &[SplitGeometry] {
        &self.splits[..self.split_len]
    }

    #[inline]
    pub(crate) fn leaf_bounds(&self, pane: PaneId) -> Option<Bounds<Pixels>> {
        self.leaves()
            .iter()
            .find_map(|(id, b)| (*id == pane).then_some(*b))
    }

    #[inline]
    pub(crate) fn split_geometry(&self, split: SplitNodeId) -> Option<SplitGeometry> {
        self.splits().iter().copied().find(|s| s.node_id == split)
    }

    #[inline]
    fn push_leaf(&mut self, pane: PaneId, bounds: Bounds<Pixels>) {
        if self.leaf_len < MAX_PANES {
            self.leaves[self.leaf_len] = (pane, bounds);
            self.leaf_len += 1;
        }
    }

    #[inline]
    fn push_split(&mut self, split: SplitGeometry) {
        if self.split_len < MAX_SPLIT_NODES {
            self.splits[self.split_len] = split;
            self.split_len += 1;
        }
    }
}

/// Computes the exact minimum `(width_px, height_px)` required at `node`'s box so every descendant
/// leaf satisfies `>= 240x160` logical pixels under the current split ratios.
pub(crate) fn min_required_extent(
    node: &LayoutRoot,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
) -> (f32, f32) {
    match node {
        LayoutRoot::Pane { .. } => (MIN_PANE_WIDTH_PX, MIN_PANE_HEIGHT_PX),
        LayoutRoot::Split {
            node_id,
            axis,
            ratio_bps,
            first,
            second,
        } => {
            let eff_bps = ratio_fn(*node_id, *ratio_bps)
                .clamp(MIN_SPLIT_RATIO_BPS, MAX_SPLIT_RATIO_BPS) as f32;
            let p = eff_bps / 10_000.0;
            let q = 1.0 - p;
            let (w1, h1) = min_required_extent(first, ratio_fn);
            let (w2, h2) = min_required_extent(second, ratio_fn);
            match axis {
                SplitAxis::Horizontal => {
                    let req_avail_w = (w1 / p).max(w2 / q);
                    (SPLIT_DIVIDER_THICKNESS_PX + req_avail_w, h1.max(h2))
                }
                SplitAxis::Vertical => {
                    let req_avail_h = (h1 / p).max(h2 / q);
                    (w1.max(w2), SPLIT_DIVIDER_THICKNESS_PX + req_avail_h)
                }
            }
        }
    }
}

/// Solves deterministic leaf and divider `Bounds<Pixels>` matching Taffy's `flex_grow` split layout.
pub(crate) fn compute_tree_geometry(
    node: &LayoutRoot,
    root_bounds: Bounds<Pixels>,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
) -> TreeGeometry {
    let mut out = TreeGeometry::default();
    collect_geometry(node, root_bounds, ratio_fn, &mut out);
    out
}

fn collect_geometry(
    node: &LayoutRoot,
    rect: Bounds<Pixels>,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
    out: &mut TreeGeometry,
) {
    match node {
        LayoutRoot::Pane { pane, .. } => {
            out.push_leaf(pane.id, rect);
        }
        LayoutRoot::Split {
            node_id,
            axis,
            ratio_bps,
            first,
            second,
        } => {
            let eff_bps =
                ratio_fn(*node_id, *ratio_bps).clamp(MIN_SPLIT_RATIO_BPS, MAX_SPLIT_RATIO_BPS);
            let p = eff_bps as f32 / 10_000.0;
            let x = rect.origin.x.as_f32();
            let y = rect.origin.y.as_f32();
            let w = rect.size.width.as_f32().max(0.0);
            let h = rect.size.height.as_f32().max(0.0);
            let d = SPLIT_DIVIDER_THICKNESS_PX;

            let (first_rect, divider_rect, second_rect) = match axis {
                SplitAxis::Horizontal => {
                    let avail = (w - d).max(0.0);
                    let w1 = avail * p;
                    let w2 = (avail - w1).max(0.0);
                    let d_w = d.min(w);
                    (
                        Bounds::new(point(px(x), px(y)), size(px(w1), px(h))),
                        Bounds::new(point(px(x + w1), px(y)), size(px(d_w), px(h))),
                        Bounds::new(point(px(x + w1 + d_w), px(y)), size(px(w2), px(h))),
                    )
                }
                SplitAxis::Vertical => {
                    let avail = (h - d).max(0.0);
                    let h1 = avail * p;
                    let h2 = (avail - h1).max(0.0);
                    let d_h = d.min(h);
                    (
                        Bounds::new(point(px(x), px(y)), size(px(w), px(h1))),
                        Bounds::new(point(px(x), px(y + h1)), size(px(w), px(d_h))),
                        Bounds::new(point(px(x), px(y + h1 + d_h)), size(px(w), px(h2))),
                    )
                }
            };

            out.push_split(SplitGeometry {
                node_id: *node_id,
                axis: *axis,
                ratio_bps: eff_bps,
                split_bounds: rect,
                divider_bounds: divider_rect,
            });
            collect_geometry(first, first_rect, ratio_fn, out);
            collect_geometry(second, second_rect, ratio_fn, out);
        }
    }
}

/// Returns `true` when `node` contains 2+ panes and `root_bounds` cannot fit every leaf at
/// `>= 240x160` logical pixels under the current split ratios.
pub(crate) fn should_use_compact_layout(
    node: &LayoutRoot,
    root_bounds: Bounds<Pixels>,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
) -> bool {
    if matches!(node, LayoutRoot::Pane { .. }) {
        return false;
    }
    let w = root_bounds.size.width.as_f32();
    let h = root_bounds.size.height.as_f32();
    if w <= 1.0 || h <= 1.0 {
        return false;
    }
    let geom = compute_tree_geometry(node, root_bounds, ratio_fn);
    geom.leaves().iter().any(|(_, b)| {
        b.size.width.as_f32() + 0.05 < MIN_PANE_WIDTH_PX
            || b.size.height.as_f32() + 0.05 < MIN_PANE_HEIGHT_PX
    })
}

/// Validates whether splitting `target_pane` in `direction` at 50/50 satisfies the `240x160`
/// logical pixel minimum for every leaf in the measured workspace pane root.
pub(crate) fn can_split_pane_in_bounds(
    node: &LayoutRoot,
    root_bounds: Option<Bounds<Pixels>>,
    target_pane: PaneId,
    direction: SplitDirection,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
) -> Result<(), &'static str> {
    let Some(bounds) = root_bounds.filter(|b| b.size.width > px(1.0) && b.size.height > px(1.0))
    else {
        return Err("Pane geometry has not been measured yet; wait one frame and retry.");
    };

    let geom = compute_tree_geometry(node, bounds, ratio_fn);
    if geom.leaves().len() >= MAX_PANES {
        return Err("Maximum of 8 split panes reached.");
    }
    if geom.leaves().iter().any(|(_, b)| {
        b.size.width.as_f32() + 0.05 < MIN_PANE_WIDTH_PX
            || b.size.height.as_f32() + 0.05 < MIN_PANE_HEIGHT_PX
    }) {
        return Err(
            "Window is too small to display existing split panes at >= 240×160 px; expand the window before splitting.",
        );
    }

    let Some(leaf_rect) = geom.leaf_bounds(target_pane) else {
        return Err("Target pane is no longer present in layout.");
    };
    let lw = leaf_rect.size.width.as_f32();
    let lh = leaf_rect.size.height.as_f32();
    match direction {
        SplitDirection::Left | SplitDirection::Right => {
            let child_w = (lw - SPLIT_DIVIDER_THICKNESS_PX) * 0.5;
            if child_w + 0.05 < MIN_PANE_WIDTH_PX || lh + 0.05 < MIN_PANE_HEIGHT_PX {
                return Err(
                    "Cannot split horizontally: each resulting pane requires at least 240×160 logical pixels.",
                );
            }
        }
        SplitDirection::Up | SplitDirection::Down => {
            let child_h = (lh - SPLIT_DIVIDER_THICKNESS_PX) * 0.5;
            if lw + 0.05 < MIN_PANE_WIDTH_PX || child_h + 0.05 < MIN_PANE_HEIGHT_PX {
                return Err(
                    "Cannot split vertically: each resulting pane requires at least 240×160 logical pixels.",
                );
            }
        }
    }
    Ok(())
}

fn find_split_subtrees(
    node: &LayoutRoot,
    split_id: SplitNodeId,
) -> Option<(&LayoutRoot, &LayoutRoot)> {
    match node {
        LayoutRoot::Pane { .. } => None,
        LayoutRoot::Split {
            node_id,
            first,
            second,
            ..
        } => {
            if *node_id == split_id {
                Some((first.as_ref(), second.as_ref()))
            } else {
                find_split_subtrees(first, split_id)
                    .or_else(|| find_split_subtrees(second, split_id))
            }
        }
    }
}

/// Computes the dynamically pixel-clamped `ratio_bps` for `split_id` at pointer `pointer`,
/// intersecting `[500, 9500]` with the pixel range that keeps EVERY leaf in both child subtrees
/// `>= 240x160` logical pixels so dragging a divider never collapses into compact mode.
pub(crate) fn clamp_divider_ratio_bps(
    node: &LayoutRoot,
    root_bounds: Bounds<Pixels>,
    split_id: SplitNodeId,
    pointer: Point<Pixels>,
    ratio_fn: &impl Fn(SplitNodeId, u16) -> u16,
) -> Option<u16> {
    let geom = compute_tree_geometry(node, root_bounds, ratio_fn);
    let split_geom = geom.split_geometry(split_id)?;
    let (first_sub, second_sub) = find_split_subtrees(node, split_id)?;
    let (w1, h1) = min_required_extent(first_sub, ratio_fn);
    let (w2, h2) = min_required_extent(second_sub, ratio_fn);

    let half_d = SPLIT_DIVIDER_THICKNESS_PX * 0.5;
    let (avail, offset, min_first_px, min_second_px) = match split_geom.axis {
        SplitAxis::Horizontal => {
            let avail =
                (split_geom.split_bounds.size.width.as_f32() - SPLIT_DIVIDER_THICKNESS_PX).max(1.0);
            let offset = pointer.x.as_f32() - split_geom.split_bounds.origin.x.as_f32() - half_d;
            (avail, offset, w1, w2)
        }
        SplitAxis::Vertical => {
            let avail = (split_geom.split_bounds.size.height.as_f32() - SPLIT_DIVIDER_THICKNESS_PX)
                .max(1.0);
            let offset = pointer.y.as_f32() - split_geom.split_bounds.origin.y.as_f32() - half_d;
            (avail, offset, h1, h2)
        }
    };

    let lo_px_bps = (((min_first_px + 0.01) * 10_000.0) / avail).ceil() as i32;
    let hi_px_bps = (10_000.0 - ((min_second_px + 0.01) * 10_000.0) / avail).floor() as i32;
    let lo = lo_px_bps.max(i32::from(MIN_SPLIT_RATIO_BPS));
    let hi = hi_px_bps.min(i32::from(MAX_SPLIT_RATIO_BPS));
    if lo > hi {
        return Some(split_geom.ratio_bps);
    }

    let raw_bps = ((offset / avail) * 10_000.0).round() as i32;
    Some(raw_bps.clamp(lo, hi) as u16)
}

/// Resolves the next or previous `PaneId` in deterministic tree traversal order (wrapping around).
pub(crate) fn adjacent_pane_id(
    layout: &SessionLayout,
    current: PaneId,
    forward: bool,
) -> Option<PaneId> {
    let mut ids = [PaneId::default(); MAX_PANES];
    let mut len = 0usize;
    let mut cur_ix = None;
    for pane in layout.panes() {
        if len >= MAX_PANES {
            break;
        }
        if pane.id == current {
            cur_ix = Some(len);
        }
        ids[len] = pane.id;
        len += 1;
    }
    if len <= 1 {
        return None;
    }
    let pos = cur_ix?;
    let next_pos = if forward {
        (pos + 1) % len
    } else {
        (pos + len - 1) % len
    };
    Some(ids[next_pos])
}

/// UI-owned per-pane scroll and prepaint measurement state nested under `GroupUiState`.
/// Active divider-resize state lives solely in Core (`EditorWorkspace::active_pane_resize`).
pub(crate) struct PaneUiState {
    strip_scrolls: HashMap<PaneId, ScrollHandle>,
    render_frame_seq: Cell<u64>,
    pub(crate) measured_root_bounds: Rc<Cell<Option<(u64, Bounds<Pixels>)>>>,
    pub(crate) prepainted_leaves: Rc<RefCell<Vec<(PaneId, Bounds<Pixels>)>>>,
    pub(crate) prepainted_bodies: Rc<RefCell<Vec<(PaneId, Bounds<Pixels>)>>>,
    pub(crate) prepainted_dividers: Rc<RefCell<Vec<(SplitNodeId, Bounds<Pixels>)>>>,
    pub(crate) compact_sync_pending: Rc<Cell<bool>>,
}

impl Default for PaneUiState {
    fn default() -> Self {
        Self {
            strip_scrolls: HashMap::with_capacity(MAX_PANES),
            render_frame_seq: Cell::new(0),
            measured_root_bounds: Rc::new(Cell::new(None)),
            prepainted_leaves: Rc::new(RefCell::new(Vec::with_capacity(MAX_PANES))),
            prepainted_bodies: Rc::new(RefCell::new(Vec::with_capacity(MAX_PANES))),
            prepainted_dividers: Rc::new(RefCell::new(Vec::with_capacity(MAX_PANES))),
            compact_sync_pending: Rc::new(Cell::new(false)),
        }
    }
}

impl PaneUiState {
    pub(crate) fn prepare_render_frame(&mut self, layout: &SessionLayout) {
        let mut live_ids = [PaneId::default(); MAX_PANES];
        let mut live_len = 0usize;
        for pane in layout.panes() {
            if live_len < MAX_PANES {
                live_ids[live_len] = pane.id;
                live_len += 1;
            }
            self.strip_scrolls.entry(pane.id).or_default();
        }
        let live_slice = &live_ids[..live_len];
        self.strip_scrolls.retain(|id, _| live_slice.contains(id));

        let next_seq = self.render_frame_seq.get().wrapping_add(1);
        self.render_frame_seq.set(next_seq);
        self.prepainted_leaves.borrow_mut().clear();
        self.prepainted_bodies.borrow_mut().clear();
        self.prepainted_dividers.borrow_mut().clear();
    }

    #[inline]
    pub(crate) fn current_frame_seq(&self) -> u64 {
        self.render_frame_seq.get()
    }

    #[inline]
    pub(crate) fn strip_scroll(&self, pane: PaneId) -> Option<&ScrollHandle> {
        self.strip_scrolls.get(&pane)
    }

    #[inline]
    pub(crate) fn root_bounds(&self) -> Option<Bounds<Pixels>> {
        self.measured_root_bounds.get().map(|(_, b)| b)
    }
}

impl EditorWorkspace {
    /// Returns the number of visible panes whose GPUI layout bounds were actually prepainted as
    /// non-empty (`width > 0 && height > 0`) on the latest frame matching the current visible set.
    /// Returns `0` before the current visible pane set has been laid out and prepainted.
    pub(crate) fn measured_visible_pane_count(&self) -> usize {
        let mut expected = [PaneId::default(); MAX_PANES];
        let mut expected_len = 0usize;
        for id in self.visible_pane_ids() {
            if expected_len < MAX_PANES {
                expected[expected_len] = id;
                expected_len += 1;
            }
        }
        let expected_slice = &expected[..expected_len];
        let leaves = self.group_ui.panes.prepainted_leaves.borrow();
        if leaves.len() != expected_slice.len() {
            return 0;
        }
        for &(pane_id, _) in leaves.iter() {
            if !expected_slice.contains(&pane_id) {
                return 0;
            }
        }
        leaves
            .iter()
            .filter(|(_, b)| b.size.width > px(0.0) && b.size.height > px(0.0))
            .count()
    }

    #[inline]
    pub(crate) fn observed_pane_leaf_bounds(&self, pane: PaneId) -> Option<Bounds<Pixels>> {
        self.group_ui
            .panes
            .prepainted_leaves
            .borrow()
            .iter()
            .find_map(|(id, b)| (*id == pane).then_some(*b))
    }

    #[inline]
    pub(crate) fn observed_pane_body_bounds(&self, pane: PaneId) -> Option<Bounds<Pixels>> {
        self.group_ui
            .panes
            .prepainted_bodies
            .borrow()
            .iter()
            .find_map(|(id, b)| (*id == pane).then_some(*b))
    }

    #[inline]
    pub(crate) fn observed_split_divider_bounds(
        &self,
        split: SplitNodeId,
    ) -> Option<Bounds<Pixels>> {
        self.group_ui
            .panes
            .prepainted_dividers
            .borrow()
            .iter()
            .find_map(|(id, b)| (*id == split).then_some(*b))
    }

    /// Reconciles `is_compact_layout()` strictly outside `render` from the measured `#workspace-pane-root`
    /// bounds, preserving the full logical split tree and ratios.
    pub(crate) fn reconcile_compact_layout_from_measurement(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.group_ui.panes.root_bounds() else {
            return;
        };
        let desired_compact = should_use_compact_layout(
            &self.session_layout().tree.root,
            bounds,
            &|split_id, fallback| self.pane_split_ratio(split_id).unwrap_or(fallback),
        );
        if desired_compact != self.is_compact_layout() {
            self.set_compact_layout(desired_compact, window, cx);
        }
    }

    pub(crate) fn sync_pane_drag_liveness(&mut self, cx: &mut Context<Self>) {
        if let Some(split_id) = self.active_pane_resize()
            && (self.is_lifecycle_busy()
                || self.is_compact_layout()
                || self.is_focus_only()
                || self.session_layout().split_ratio(split_id).is_none())
        {
            let _ = self.cancel_pane_resize(cx);
        }
    }

    pub(crate) fn cancel_divider_drag(&mut self, cx: &mut Context<Self>) -> bool {
        if self.cancel_pane_resize(cx) {
            self.status_flash = Some(("Split resize cancelled".to_string(), Instant::now()));
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(crate) fn begin_divider_drag(
        &mut self,
        split_id: SplitNodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_compact_layout() || self.is_focus_only() {
            return;
        }
        match self.begin_pane_resize(split_id, cx) {
            Ok(()) => {
                cx.notify();
            }
            Err(err) => {
                self.notify_group_service_error(&err, window, cx);
            }
        }
    }

    pub(crate) fn update_divider_drag_at(
        &mut self,
        pointer: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(split_id) = self.active_pane_resize() else {
            return;
        };
        let Some(root_bounds) = self.group_ui.panes.root_bounds() else {
            let _ = self.cancel_pane_resize(cx);
            return;
        };
        let Some(clamped_bps) = clamp_divider_ratio_bps(
            &self.session_layout().tree.root,
            root_bounds,
            split_id,
            pointer,
            &|id, fallback| self.pane_split_ratio(id).unwrap_or(fallback),
        ) else {
            let _ = self.cancel_pane_resize(cx);
            return;
        };
        if self.update_pane_resize(split_id, clamped_bps, cx).is_err() {
            let _ = self.cancel_pane_resize(cx);
        }
    }

    pub(crate) fn commit_divider_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_pane_resize().is_some()
            && let Err(err) = self.finish_pane_resize(true, window, cx)
        {
            self.notify_group_service_error(&err, window, cx);
        }
    }

    pub(crate) fn run_ui_pane_command(
        &mut self,
        command: PaneCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneCommandResult> {
        match self.apply_pane_command(command, window, cx) {
            Ok(res) => Some(res),
            Err(err) => {
                self.notify_group_service_error(&err, window, cx);
                None
            }
        }
    }

    /// Validates interactive minimum geometry (`>= 240x160` per leaf) before dispatching an atomic
    /// `PaneCommand::Split` (with optional tab move). Never mutates layout on rejection.
    pub(crate) fn request_interactive_split(
        &mut self,
        pane: PaneId,
        direction: SplitDirection,
        moving: Option<PaneMove>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<PaneId> {
        if self.session_layout().panes().count() >= MAX_PANES {
            let msg = format!("Maximum of {MAX_PANES} split panes reached.");
            self.status_flash = Some((msg.clone(), Instant::now()));
            window.push_notification(
                Notification::warning(msg).placement(Anchor::BottomRight),
                cx,
            );
            cx.notify();
            return None;
        }

        let root_bounds = self.group_ui.panes.root_bounds();
        if let Err(reason) = can_split_pane_in_bounds(
            &self.session_layout().tree.root,
            root_bounds,
            pane,
            direction,
            &|id, fallback| self.pane_split_ratio(id).unwrap_or(fallback),
        ) {
            self.status_flash = Some((reason.to_string(), Instant::now()));
            window.push_notification(
                Notification::warning(reason).placement(Anchor::BottomRight),
                cx,
            );
            cx.notify();
            return None;
        }

        let res = self.run_ui_pane_command(
            PaneCommand::Split {
                pane,
                direction,
                moving,
            },
            window,
            cx,
        )?;
        if self.is_focus_only() {
            self.set_focus_only(false, window, cx);
        }
        res.created_pane
    }

    pub(crate) fn move_view_to_pane(
        &mut self,
        target: ViewTarget,
        source: PaneId,
        destination: PaneId,
        group_override: Option<GroupId>,
        before: Option<editor_services::ViewId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let group = match group_override {
            Some(g) => g,
            None => match self.session_layout().resolve(target) {
                Ok(lv) => lv.group_id,
                Err(err) => {
                    self.notify_group_service_error(&err, window, cx);
                    return false;
                }
            },
        };
        if source == destination && group_override.is_none() && before.is_none() {
            return self
                .run_ui_pane_command(
                    PaneCommand::Activate {
                        pane: destination,
                        view: Some(target),
                    },
                    window,
                    cx,
                )
                .is_some();
        }
        self.run_ui_pane_command(
            PaneCommand::MoveView {
                target,
                source,
                destination,
                group,
                before,
            },
            window,
            cx,
        )
        .is_some()
    }

    pub(crate) fn move_view_to_adjacent_pane(
        &mut self,
        target: ViewTarget,
        source: PaneId,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dest_pane) = adjacent_pane_id(self.session_layout(), source, forward) else {
            let msg = "Split a pane first to move tabs between panes.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg).placement(Anchor::BottomRight), cx);
            cx.notify();
            return;
        };
        let _ = self.move_view_to_pane(target, source, dest_pane, None, None, window, cx);
    }

    /// Center-body tab drop: activates without reordering when `drag.source_pane == dest_pane` (B3),
    /// and moves across panes preserving the tab's `GroupId` when `drag.source_pane != dest_pane`.
    pub(crate) fn complete_tab_drop_pane_center(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_drag_preview();
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        if drag.source_pane == dest_pane {
            let _ = self.run_ui_pane_command(
                PaneCommand::Activate {
                    pane: dest_pane,
                    view: Some(drag.target),
                },
                window,
                cx,
            );
            return;
        }
        let _ = self.move_view_to_pane(
            drag.target,
            drag.source_pane,
            dest_pane,
            None,
            None,
            window,
            cx,
        );
    }

    /// Trailing tab-strip drop: moves tab to `dest_pane` preserving its `GroupId` on cross-pane
    /// drops, or activates without reordering on same-pane drops.
    pub(crate) fn complete_tab_drop_pane_strip_tail(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.complete_tab_drop_pane_center(drag, dest_pane, window, cx);
    }

    /// Edge-zone tab drop: validates interactive geometry and dispatches ONE atomic
    /// `PaneCommand::Split` with `PaneMove`. Never leaves an orphan split if move fails.
    pub(crate) fn complete_tab_drop_pane_edge(
        &mut self,
        drag: DraggedTabView,
        dest_pane: PaneId,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_drag_preview();
        if drag.workspace_entity_id != cx.entity_id() {
            cx.notify();
            return;
        }
        let _ = self.request_interactive_split(
            dest_pane,
            direction,
            Some(PaneMove {
                target: drag.target,
                source: drag.source_pane,
            }),
            window,
            cx,
        );
    }

    pub(crate) fn execute_pane_palette_action(
        &mut self,
        action: PanePaletteAction,
        _origin: OverlayOrigin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            PanePaletteAction::Split {
                pane,
                direction,
                moving,
            } => {
                let mv = moving.map(|(source, target)| PaneMove { target, source });
                let _ = self.request_interactive_split(pane, direction, mv, window, cx);
            }
            PanePaletteAction::FocusNextPane => {
                if let Err(err) = self.cycle_pane(true, window, cx) {
                    self.notify_group_service_error(&err, window, cx);
                }
            }
            PanePaletteAction::FocusPrevPane => {
                if let Err(err) = self.cycle_pane(false, window, cx) {
                    self.notify_group_service_error(&err, window, cx);
                }
            }
            PanePaletteAction::ActivatePane { pane } => {
                if let Err(err) = self.activate_pane(pane, window, cx) {
                    self.notify_group_service_error(&err, window, cx);
                }
            }
            PanePaletteAction::MoveTabToPane {
                target,
                source,
                destination,
            } => {
                let _ = self.move_view_to_pane(target, source, destination, None, None, window, cx);
            }
            PanePaletteAction::MoveTabAdjacentPane {
                target,
                source,
                forward,
            } => {
                self.move_view_to_adjacent_pane(target, source, forward, window, cx);
            }
            PanePaletteAction::Equalize => {
                let _ = self.run_ui_pane_command(PaneCommand::Equalize, window, cx);
            }
            PanePaletteAction::SetFocusOnly { enabled } => {
                self.set_focus_only(enabled, window, cx);
            }
            PanePaletteAction::ClosePaneMerge { pane } => {
                let _ = self.run_ui_pane_command(PaneCommand::Close { pane }, window, cx);
            }
            PanePaletteAction::ClosePaneTabs { pane } => {
                self.request_close_pane_tabs(pane, window, cx);
            }
        }
    }

    // --- Keyboard & Native Menu Action Listeners ---

    pub(crate) fn on_split_pane_right(
        &mut self,
        _: &SplitPaneRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let _ = self.request_interactive_split(pane, SplitDirection::Right, None, window, cx);
    }

    pub(crate) fn on_split_pane_down(
        &mut self,
        _: &SplitPaneDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let _ = self.request_interactive_split(pane, SplitDirection::Down, None, window, cx);
    }

    pub(crate) fn on_split_pane_left(
        &mut self,
        _: &SplitPaneLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let _ = self.request_interactive_split(pane, SplitDirection::Left, None, window, cx);
    }

    pub(crate) fn on_split_pane_up(
        &mut self,
        _: &SplitPaneUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let _ = self.request_interactive_split(pane, SplitDirection::Up, None, window, cx);
    }

    fn split_and_move_active_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let Some(target) = self.pane_active_target(pane) else {
            let msg = "Active pane has no open tab to move; use Split Pane for an empty split.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            window.push_notification(Notification::info(msg).placement(Anchor::BottomRight), cx);
            cx.notify();
            return;
        };
        let _ = self.request_interactive_split(
            pane,
            direction,
            Some(PaneMove {
                target,
                source: pane,
            }),
            window,
            cx,
        );
    }

    pub(crate) fn on_split_and_move_active_tab_right(
        &mut self,
        _: &SplitAndMoveActiveTabRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_and_move_active_in_direction(SplitDirection::Right, window, cx);
    }

    pub(crate) fn on_split_and_move_active_tab_down(
        &mut self,
        _: &SplitAndMoveActiveTabDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_and_move_active_in_direction(SplitDirection::Down, window, cx);
    }

    pub(crate) fn on_split_and_move_active_tab_left(
        &mut self,
        _: &SplitAndMoveActiveTabLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_and_move_active_in_direction(SplitDirection::Left, window, cx);
    }

    pub(crate) fn on_split_and_move_active_tab_up(
        &mut self,
        _: &SplitAndMoveActiveTabUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.split_and_move_active_in_direction(SplitDirection::Up, window, cx);
    }

    pub(crate) fn on_focus_next_pane(
        &mut self,
        _: &FocusNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.cycle_pane(true, window, cx) {
            self.notify_group_service_error(&err, window, cx);
        }
    }

    pub(crate) fn on_focus_prev_pane(
        &mut self,
        _: &FocusPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.cycle_pane(false, window, cx) {
            self.notify_group_service_error(&err, window, cx);
        }
    }

    pub(crate) fn on_move_active_tab_to_next_pane(
        &mut self,
        _: &MoveActiveTabToNextPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.active_pane_id();
        let Some(target) = self.pane_active_target(source) else {
            return;
        };
        self.move_view_to_adjacent_pane(target, source, true, window, cx);
    }

    pub(crate) fn on_move_active_tab_to_prev_pane(
        &mut self,
        _: &MoveActiveTabToPrevPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = self.active_pane_id();
        let Some(target) = self.pane_active_target(source) else {
            return;
        };
        self.move_view_to_adjacent_pane(target, source, false, window, cx);
    }

    pub(crate) fn on_equalize_panes(
        &mut self,
        _: &EqualizePanes,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self.run_ui_pane_command(PaneCommand::Equalize, window, cx);
    }

    pub(crate) fn on_toggle_focus_active_pane(
        &mut self,
        _: &ToggleFocusActivePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.session_layout().panes().count() <= 1 && !self.is_focus_only() {
            let msg = "Split a pane first to toggle Focus-Only Pane mode.";
            self.status_flash = Some((msg.to_string(), Instant::now()));
            cx.notify();
            return;
        }
        self.set_focus_only(!self.is_focus_only(), window, cx);
    }

    pub(crate) fn on_close_active_pane(
        &mut self,
        _: &CloseActivePane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        let _ = self.run_ui_pane_command(PaneCommand::Close { pane }, window, cx);
    }

    pub(crate) fn on_close_active_pane_tabs(
        &mut self,
        _: &CloseActivePaneTabs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.active_pane_id();
        self.request_close_pane_tabs(pane, window, cx);
    }
}

/// Lazily builds the per-pane options dropdown menu for `pane_id`.
/// Explicitly distinguishes `Close Pane (Merge Tabs into Adjacent Pane)` from `Close All Tabs in This Pane…`.
pub(crate) fn build_pane_options_menu(
    menu: PopupMenu,
    workspace: Entity<EditorWorkspace>,
    pane_id: PaneId,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let (
        pane_index_1based,
        pane_count,
        pane_tab_count,
        pane_selected_target,
        focus_only,
        compact_layout,
        lifecycle_busy,
        can_split_h,
        can_split_v,
        other_panes,
    ) = {
        let ws = workspace.read(cx);
        let layout = ws.session_layout();
        let mut idx = 0usize;
        let mut total = 0usize;
        let mut others = Vec::new();
        for (i, p) in layout.panes().enumerate() {
            total += 1;
            if p.id == pane_id {
                idx = i + 1;
            } else {
                others.push((i + 1, p.id, p.tabs.len()));
            }
        }
        let Some(pane) = layout.pane(pane_id) else {
            return menu.min_w(px(210.)).label("Pane is no longer open");
        };
        let root_bounds = ws.group_ui.panes.root_bounds();
        let ok_h = total < MAX_PANES
            && can_split_pane_in_bounds(
                &layout.tree.root,
                root_bounds,
                pane_id,
                SplitDirection::Right,
                &|id, fb| ws.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();
        let ok_v = total < MAX_PANES
            && can_split_pane_in_bounds(
                &layout.tree.root,
                root_bounds,
                pane_id,
                SplitDirection::Down,
                &|id, fb| ws.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();
        (
            idx,
            total,
            pane.tabs.len(),
            ws.pane_active_target(pane_id),
            ws.is_focus_only(),
            ws.is_compact_layout(),
            ws.is_lifecycle_busy(),
            ok_h,
            ok_v,
            others,
        )
    };

    let ws_sr = workspace.clone();
    let ws_sd = workspace.clone();
    let ws_sl = workspace.clone();
    let ws_su = workspace.clone();
    let ws_eq = workspace.clone();
    let ws_fo = workspace.clone();
    let ws_merge = workspace.clone();
    let ws_close_tabs = workspace.clone();

    let mut m = menu
        .min_w(px(260.))
        .label(if compact_layout {
            format!("Pane {pane_index_1based} of {pane_count} · Compact Mode")
        } else {
            format!("Pane {pane_index_1based} of {pane_count} ({pane_tab_count} tabs)")
        })
        .item(
            PopupMenuItem::new("Split Pane Right")
                .disabled(lifecycle_busy || !can_split_h)
                .on_click(move |_, window, cx| {
                    ws_sr.update(cx, |ws, cx| {
                        let _ = ws.request_interactive_split(
                            pane_id,
                            SplitDirection::Right,
                            None,
                            window,
                            cx,
                        );
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Split Pane Down")
                .disabled(lifecycle_busy || !can_split_v)
                .on_click(move |_, window, cx| {
                    ws_sd.update(cx, |ws, cx| {
                        let _ = ws.request_interactive_split(
                            pane_id,
                            SplitDirection::Down,
                            None,
                            window,
                            cx,
                        );
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Split Pane Left")
                .disabled(lifecycle_busy || !can_split_h)
                .on_click(move |_, window, cx| {
                    ws_sl.update(cx, |ws, cx| {
                        let _ = ws.request_interactive_split(
                            pane_id,
                            SplitDirection::Left,
                            None,
                            window,
                            cx,
                        );
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Split Pane Up")
                .disabled(lifecycle_busy || !can_split_v)
                .on_click(move |_, window, cx| {
                    ws_su.update(cx, |ws, cx| {
                        let _ = ws.request_interactive_split(
                            pane_id,
                            SplitDirection::Up,
                            None,
                            window,
                            cx,
                        );
                    });
                }),
        );

    if let Some(target) = pane_selected_target {
        let ws_mv_r = workspace.clone();
        let ws_mv_d = workspace.clone();
        let ws_mv_l = workspace.clone();
        let ws_mv_u = workspace.clone();
        m = m
            .separator()
            .item(
                PopupMenuItem::new("Split & Move Active Tab Right")
                    .disabled(lifecycle_busy || !can_split_h)
                    .on_click(move |_, window, cx| {
                        ws_mv_r.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                pane_id,
                                SplitDirection::Right,
                                Some(PaneMove {
                                    target,
                                    source: pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Active Tab Down")
                    .disabled(lifecycle_busy || !can_split_v)
                    .on_click(move |_, window, cx| {
                        ws_mv_d.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                pane_id,
                                SplitDirection::Down,
                                Some(PaneMove {
                                    target,
                                    source: pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Active Tab Left")
                    .disabled(lifecycle_busy || !can_split_h)
                    .on_click(move |_, window, cx| {
                        ws_mv_l.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                pane_id,
                                SplitDirection::Left,
                                Some(PaneMove {
                                    target,
                                    source: pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            )
            .item(
                PopupMenuItem::new("Split & Move Active Tab Up")
                    .disabled(lifecycle_busy || !can_split_v)
                    .on_click(move |_, window, cx| {
                        ws_mv_u.update(cx, |ws, cx| {
                            let _ = ws.request_interactive_split(
                                pane_id,
                                SplitDirection::Up,
                                Some(PaneMove {
                                    target,
                                    source: pane_id,
                                }),
                                window,
                                cx,
                            );
                        });
                    }),
            );

        if !other_panes.is_empty() {
            let ws_sub = workspace.clone();
            m = m.submenu(
                "Move Active Tab to Pane",
                _window,
                cx,
                move |mut sub, _w, _cx| {
                    sub = sub.min_w(px(195.));
                    for &(other_num, other_id, other_tabs) in &other_panes {
                        let ws_dest = ws_sub.clone();
                        sub = sub.item(
                            PopupMenuItem::element(move |_w, cx| {
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .justify_between()
                                    .w_full()
                                    .child(format!("Pane {other_num}"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("{other_tabs} tabs")),
                                    )
                            })
                            .disabled(lifecycle_busy)
                            .on_click(move |_, window, cx| {
                                ws_dest.update(cx, |ws, cx| {
                                    let _ = ws.move_view_to_pane(
                                        target, pane_id, other_id, None, None, window, cx,
                                    );
                                });
                            }),
                        );
                    }
                    sub
                },
            );
        }
    }

    m.separator()
        .item(
            PopupMenuItem::new("Equalize Split Pane Sizes")
                .disabled(lifecycle_busy || pane_count <= 1)
                .on_click(move |_, window, cx| {
                    ws_eq.update(cx, |ws, cx| {
                        let _ = ws.run_ui_pane_command(PaneCommand::Equalize, window, cx);
                    });
                }),
        )
        .item(
            PopupMenuItem::new(if focus_only {
                "Exit Focus-Only Mode (Show All Panes)"
            } else {
                "Focus Active Pane Only"
            })
            .checked(focus_only)
            .disabled(pane_count <= 1 && !focus_only)
            .on_click(move |_, window, cx| {
                ws_fo.update(cx, |ws, cx| {
                    if ws.active_pane_id() != pane_id
                        && ws.activate_pane(pane_id, window, cx).is_err()
                    {
                        return;
                    }
                    ws.set_focus_only(!focus_only, window, cx);
                });
            }),
        )
        .separator()
        .item(
            PopupMenuItem::new("Close Pane (Merge Tabs into Adjacent Pane)")
                .disabled(lifecycle_busy || pane_count <= 1)
                .on_click(move |_, window, cx| {
                    ws_merge.update(cx, |ws, cx| {
                        let _ = ws.run_ui_pane_command(
                            PaneCommand::Close { pane: pane_id },
                            window,
                            cx,
                        );
                    });
                }),
        )
        .item(
            PopupMenuItem::new("Close All Tabs in This Pane…")
                .disabled(lifecycle_busy || pane_tab_count == 0)
                .on_click(move |_, window, cx| {
                    ws_close_tabs.update(cx, |ws, cx| {
                        ws.request_close_pane_tabs(pane_id, window, cx);
                    });
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_and_split_element_ids_are_collision_free_across_128_bit_identities() {
        let p1 = PaneId(1);
        let p2 = PaneId((1u128 << 64) | 1);
        assert_ne!(pane_elem_id(*b"pane", p1), pane_elem_id(*b"pane", p2));
        assert_ne!(pane_elem_id(*b"pane", p1), pane_elem_id(*b"pbdy", p1));

        let s1 = SplitNodeId(7);
        let s2 = SplitNodeId((9u128 << 64) | 7);
        assert_ne!(split_elem_id(*b"sdiv", s1), split_elem_id(*b"sdiv", s2));
    }

    #[test]
    fn compact_threshold_and_interactive_split_enforce_240x160_minimum_leaf_geometry() {
        let mut layout = SessionLayout::default();
        let p1 = layout.active_pane_id();
        let wide_box = Bounds::new(point(px(0.0), px(0.0)), size(px(1200.0), px(760.0)));
        assert!(!should_use_compact_layout(
            &layout.tree.root,
            wide_box,
            &|_, r| r
        ));
        assert!(
            can_split_pane_in_bounds(
                &layout.tree.root,
                Some(wide_box),
                p1,
                SplitDirection::Right,
                &|_, r| r
            )
            .is_ok()
        );

        // 480px width cannot fit two 240px panes plus a 5px divider (needs 485px).
        let narrow_box = Bounds::new(point(px(0.0), px(0.0)), size(px(480.0), px(400.0)));
        assert!(
            can_split_pane_in_bounds(
                &layout.tree.root,
                Some(narrow_box),
                p1,
                SplitDirection::Right,
                &|_, r| r
            )
            .is_err()
        );
        assert!(
            can_split_pane_in_bounds(
                &layout.tree.root,
                Some(narrow_box),
                p1,
                SplitDirection::Down,
                &|_, r| r
            )
            .is_ok()
        );

        // Build a 4x2 8-pane layout and verify 1200x760 keeps all 8 leaves >= 240x160,
        // while shrinking to 900x760 activates compact mode without mutating the tree.
        let p2 = layout
            .apply_pane_command(PaneCommand::Split {
                pane: p1,
                direction: SplitDirection::Right,
                moving: None,
            })
            .unwrap()
            .created_pane
            .unwrap();
        let p3 = layout
            .apply_pane_command(PaneCommand::Split {
                pane: p1,
                direction: SplitDirection::Left,
                moving: None,
            })
            .unwrap()
            .created_pane
            .unwrap();
        let p4 = layout
            .apply_pane_command(PaneCommand::Split {
                pane: p2,
                direction: SplitDirection::Right,
                moving: None,
            })
            .unwrap()
            .created_pane
            .unwrap();
        for col in [p3, p1, p2, p4] {
            layout
                .apply_pane_command(PaneCommand::Split {
                    pane: col,
                    direction: SplitDirection::Down,
                    moving: None,
                })
                .unwrap();
        }
        assert_eq!(layout.panes().count(), 8);
        assert!(!should_use_compact_layout(
            &layout.tree.root,
            wide_box,
            &|_, r| r
        ));
        let geom_8 = compute_tree_geometry(&layout.tree.root, wide_box, &|_, r| r);
        assert_eq!(geom_8.leaves().len(), 8);
        assert_eq!(geom_8.splits().len(), 7);
        for (_, leaf_b) in geom_8.leaves() {
            assert!(leaf_b.size.width >= px(MIN_PANE_WIDTH_PX));
            assert!(leaf_b.size.height >= px(MIN_PANE_HEIGHT_PX));
        }

        let shrunk = Bounds::new(point(px(0.0), px(0.0)), size(px(900.0), px(760.0)));
        assert!(should_use_compact_layout(
            &layout.tree.root,
            shrunk,
            &|_, r| r
        ));
    }

    #[test]
    fn divider_clamp_preserves_nested_subtree_240x160_leaf_minimums() {
        let mut layout = SessionLayout::default();
        let p1 = layout.active_pane_id();
        let p2 = layout
            .apply_pane_command(PaneCommand::Split {
                pane: p1,
                direction: SplitDirection::Right,
                moving: None,
            })
            .unwrap()
            .created_pane
            .unwrap();
        // Split left half p1 again horizontally -> left subtree now needs 240 + 5 + 240 = 485px!
        layout
            .apply_pane_command(PaneCommand::Split {
                pane: p1,
                direction: SplitDirection::Right,
                moving: None,
            })
            .unwrap();
        let _ = p2;

        let LayoutRoot::Split {
            node_id: outer_split,
            ..
        } = layout.tree.root
        else {
            panic!("expected outer split");
        };

        let root_box = Bounds::new(point(px(0.0), px(0.0)), size(px(1005.0), px(500.0)));
        // Dragging outer divider far left to x=10px must clamp so both nested left leaves stay >= 240px!
        let min_clamped = clamp_divider_ratio_bps(
            &layout.tree.root,
            root_box,
            outer_split,
            point(px(10.0), px(250.0)),
            &|_, r| r,
        )
        .unwrap();
        let geom_min = compute_tree_geometry(&layout.tree.root, root_box, &|id, r| {
            if id == outer_split { min_clamped } else { r }
        });
        for (_, b) in geom_min.leaves() {
            assert!(
                b.size.width.as_f32() + 0.05 >= MIN_PANE_WIDTH_PX,
                "nested leaf width {} dropped below 240px at clamped bps {min_clamped}",
                b.size.width.as_f32()
            );
        }

        // Dragging outer divider far right to x=995px must keep right leaf >= 240px!
        let max_clamped = clamp_divider_ratio_bps(
            &layout.tree.root,
            root_box,
            outer_split,
            point(px(995.0), px(250.0)),
            &|_, r| r,
        )
        .unwrap();
        let geom_max = compute_tree_geometry(&layout.tree.root, root_box, &|id, r| {
            if id == outer_split { max_clamped } else { r }
        });
        for (_, b) in geom_max.leaves() {
            assert!(
                b.size.width.as_f32() + 0.05 >= MIN_PANE_WIDTH_PX,
                "right leaf width {} dropped below 240px at clamped bps {max_clamped}",
                b.size.width.as_f32()
            );
        }
    }
}
