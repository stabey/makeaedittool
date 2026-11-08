use crate::{
    ContentIdentity, DocMeta, DocumentId, EncodingSpec, ErrorKind, LifecycleGen, LineEnding,
    RecoveryWarning, Revision, ServiceError, ViewState,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{HashMap, HashSet};

pub const MAX_CUSTOM_GROUPS: usize = 128;
pub const MAX_GROUP_NAME_BYTES: usize = 128;
pub const MAX_SESSION_VIEWS: usize = 1024;
pub const MAX_LAYOUT_TREE_BYTES: usize = 512 * 1024;
pub const MAX_METADATA_DECODE_BYTES: usize = 64 * 1024;
pub const LAYOUT_FORMAT_VERSION: u32 = 2;
pub const MAX_PANES: usize = 8;
pub const MAX_LAYOUT_NODES: usize = 15;
pub const MIN_SPLIT_RATIO_BPS: u16 = 500;
pub const MAX_SPLIT_RATIO_BPS: u16 = 9500;

macro_rules! identity {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u128);
        impl $name {
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().as_u128())
            }
            pub fn key(self) -> String {
                format!("{:032x}", self.0)
            }
            pub fn from_key(value: &str) -> Result<Self, ServiceError> {
                if value.len() != 32
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(ServiceError::new(
                        ErrorKind::Corrupt,
                        "invalid stable identity",
                    ));
                }
                u128::from_str_radix(value, 16)
                    .map(Self)
                    .map_err(|e| ServiceError::new(ErrorKind::Corrupt, e.to_string()))
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.key())
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::from_key(&String::deserialize(deserializer)?)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}
identity!(ViewId);
identity!(GroupId);
identity!(PaneId);
identity!(SplitNodeId);
impl GroupId {
    pub const UNGROUPED: Self = Self(0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DocumentKey {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ViewTarget {
    pub view_id: ViewId,
    pub document: DocumentKey,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupColor {
    #[default]
    Neutral,
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
}
impl GroupColor {
    pub const ALL: [Self; 9] = [
        Self::Neutral,
        Self::Red,
        Self::Orange,
        Self::Yellow,
        Self::Green,
        Self::Teal,
        Self::Blue,
        Self::Purple,
        Self::Pink,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::Neutral => "Neutral",
            Self::Red => "Red",
            Self::Orange => "Orange",
            Self::Yellow => "Yellow",
            Self::Green => "Green",
            Self::Teal => "Teal",
            Self::Blue => "Blue",
            Self::Purple => "Purple",
            Self::Pink => "Pink",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabGroup {
    pub id: GroupId,
    pub name: String,
    pub color: GroupColor,
    pub collapsed: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneLayout {
    pub id: PaneId,
    pub tabs: Vec<ViewId>,
    pub active_view: Option<ViewId>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutRoot {
    Pane {
        node_id: SplitNodeId,
        pane: PaneLayout,
    },
    Split {
        node_id: SplitNodeId,
        axis: SplitAxis,
        ratio_bps: u16,
        first: Box<LayoutRoot>,
        second: Box<LayoutRoot>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutTree {
    pub format_version: u32,
    pub groups: Vec<TabGroup>,
    pub active_pane: PaneId,
    pub root: LayoutRoot,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutView {
    pub id: ViewId,
    pub document: DocumentKey,
    pub group_id: GroupId,
    pub view: ViewState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    pub tree: LayoutTree,
    pub views: HashMap<ViewId, LayoutView>,
}
impl Default for SessionLayout {
    fn default() -> Self {
        let pane_id = PaneId::new();
        Self {
            tree: LayoutTree {
                format_version: LAYOUT_FORMAT_VERSION,
                groups: vec![TabGroup {
                    id: GroupId::UNGROUPED,
                    name: "Ungrouped".into(),
                    color: GroupColor::Neutral,
                    collapsed: false,
                }],
                active_pane: pane_id,
                root: LayoutRoot::Pane {
                    node_id: SplitNodeId::new(),
                    pane: PaneLayout {
                        id: pane_id,
                        tabs: Vec::new(),
                        active_view: None,
                    },
                },
            },
            views: HashMap::new(),
        }
    }
}
struct PaneIter<'a> {
    stack: [Option<&'a LayoutRoot>; MAX_LAYOUT_NODES],
    len: usize,
}
impl<'a> PaneIter<'a> {
    fn new(root: &'a LayoutRoot) -> Self {
        let mut stack = [None; MAX_LAYOUT_NODES];
        stack[0] = Some(root);
        Self { stack, len: 1 }
    }
}
impl<'a> Iterator for PaneIter<'a> {
    type Item = &'a PaneLayout;
    fn next(&mut self) -> Option<Self::Item> {
        while self.len > 0 {
            self.len -= 1;
            match self.stack[self.len].take()? {
                LayoutRoot::Pane { pane, .. } => return Some(pane),
                LayoutRoot::Split { first, second, .. } => {
                    self.stack[self.len] = Some(second);
                    self.stack[self.len + 1] = Some(first);
                    self.len += 2;
                }
            }
        }
        None
    }
}
impl LayoutRoot {
    fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneLayout> {
        match self {
            Self::Pane { pane, .. } => (pane.id == id).then_some(pane),
            Self::Split { first, second, .. } => first.pane_mut(id).or_else(|| second.pane_mut(id)),
        }
    }
    fn for_each_pane_mut(&mut self, f: &mut impl FnMut(&mut PaneLayout)) {
        match self {
            Self::Pane { pane, .. } => f(pane),
            Self::Split { first, second, .. } => {
                first.for_each_pane_mut(f);
                second.for_each_pane_mut(f);
            }
        }
    }
    fn split(&self, id: SplitNodeId) -> Option<(SplitAxis, u16)> {
        match self {
            Self::Pane { .. } => None,
            Self::Split {
                node_id,
                axis,
                ratio_bps,
                first,
                second,
            } => {
                if *node_id == id {
                    Some((*axis, *ratio_bps))
                } else {
                    first.split(id).or_else(|| second.split(id))
                }
            }
        }
    }
    fn ratio_mut(&mut self, id: SplitNodeId) -> Option<&mut u16> {
        match self {
            Self::Pane { .. } => None,
            Self::Split {
                node_id,
                ratio_bps,
                first,
                second,
                ..
            } => {
                if *node_id == id {
                    Some(ratio_bps)
                } else {
                    first.ratio_mut(id).or_else(|| second.ratio_mut(id))
                }
            }
        }
    }
    fn placeholder() -> Self {
        Self::Pane {
            node_id: SplitNodeId(0),
            pane: PaneLayout {
                id: PaneId(0),
                tabs: Vec::new(),
                active_view: None,
            },
        }
    }
    fn split_pane(&mut self, id: PaneId, direction: SplitDirection, new_pane: PaneId) {
        match self {
            Self::Pane { pane, .. } if pane.id == id => {
                let old = Box::new(std::mem::replace(self, Self::placeholder()));
                let new = Box::new(Self::Pane {
                    node_id: SplitNodeId::new(),
                    pane: PaneLayout {
                        id: new_pane,
                        tabs: Vec::new(),
                        active_view: None,
                    },
                });
                let axis = match direction {
                    SplitDirection::Left | SplitDirection::Right => SplitAxis::Horizontal,
                    SplitDirection::Up | SplitDirection::Down => SplitAxis::Vertical,
                };
                let (first, second) = match direction {
                    SplitDirection::Left | SplitDirection::Up => (new, old),
                    SplitDirection::Right | SplitDirection::Down => (old, new),
                };
                *self = Self::Split {
                    node_id: SplitNodeId::new(),
                    axis,
                    ratio_bps: 5000,
                    first,
                    second,
                };
            }
            Self::Split { first, second, .. } => {
                first.split_pane(id, direction, new_pane);
                second.split_pane(id, direction, new_pane);
            }
            _ => {}
        }
    }
    fn boundary_pane(&self, first_boundary: bool) -> PaneId {
        match self {
            Self::Pane { pane, .. } => pane.id,
            Self::Split { first, second, .. } => {
                if first_boundary {
                    first.boundary_pane(true)
                } else {
                    second.boundary_pane(false)
                }
            }
        }
    }
    fn remove_pane(&mut self, id: PaneId) -> Option<(PaneLayout, PaneId)> {
        let Self::Split { first, second, .. } = self else {
            return None;
        };
        let first_match = matches!(first.as_ref(), Self::Pane { pane, .. } if pane.id == id);
        let second_match = matches!(second.as_ref(), Self::Pane { pane, .. } if pane.id == id);
        if first_match || second_match {
            let old = std::mem::replace(self, Self::placeholder());
            let Self::Split { first, second, .. } = old else {
                unreachable!()
            };
            let (removed, sibling) = if first_match {
                (first, second)
            } else {
                (second, first)
            };
            let receiver = sibling.boundary_pane(first_match);
            let Self::Pane { pane, .. } = *removed else {
                unreachable!()
            };
            *self = *sibling;
            Some((pane, receiver))
        } else {
            first.remove_pane(id).or_else(|| second.remove_pane(id))
        }
    }
    fn equalize(&mut self) -> (usize, bool) {
        match self {
            Self::Pane { .. } => (1, false),
            Self::Split {
                ratio_bps,
                first,
                second,
                ..
            } => {
                let (a, ac) = first.equalize();
                let (b, bc) = second.equalize();
                let next = ((a * 10_000 + (a + b) / 2) / (a + b)) as u16;
                let changed = ac || bc || *ratio_bps != next;
                *ratio_bps = next;
                (a + b, changed)
            }
        }
    }
}
fn validate_nodes(
    node: &LayoutRoot,
    depth: usize,
    nodes: &mut HashSet<SplitNodeId>,
    panes: &mut HashSet<PaneId>,
) -> Result<(), ServiceError> {
    if depth > MAX_LAYOUT_NODES || nodes.len() >= MAX_LAYOUT_NODES {
        return Err(invalid("layout node limit exceeded"));
    }
    let id = match node {
        LayoutRoot::Pane { node_id, .. } | LayoutRoot::Split { node_id, .. } => *node_id,
    };
    if id.0 == 0 || !nodes.insert(id) {
        return Err(invalid("duplicate or zero node identity"));
    }
    match node {
        LayoutRoot::Pane { pane, .. } => {
            if pane.id.0 == 0 || !panes.insert(pane.id) || panes.len() > MAX_PANES {
                return Err(invalid("duplicate, zero, or excess pane identity"));
            }
        }
        LayoutRoot::Split {
            ratio_bps,
            first,
            second,
            ..
        } => {
            if !(MIN_SPLIT_RATIO_BPS..=MAX_SPLIT_RATIO_BPS).contains(ratio_bps) {
                return Err(invalid("split ratio out of range"));
            }
            validate_nodes(first, depth + 1, nodes, panes)?;
            validate_nodes(second, depth + 1, nodes, panes)?;
        }
    }
    Ok(())
}

impl SessionLayout {
    pub fn panes(&self) -> impl Iterator<Item = &PaneLayout> {
        PaneIter::new(&self.tree.root)
    }
    pub fn pane(&self, id: PaneId) -> Option<&PaneLayout> {
        self.panes().find(|pane| pane.id == id)
    }
    pub fn pane_mut(&mut self, id: PaneId) -> Option<&mut PaneLayout> {
        self.tree.root.pane_mut(id)
    }
    pub fn pane_for_view(&self, id: ViewId) -> Option<PaneId> {
        self.panes()
            .find(|pane| pane.tabs.contains(&id))
            .map(|pane| pane.id)
    }
    pub fn active_pane_id(&self) -> PaneId {
        self.tree.active_pane
    }
    pub fn active_pane(&self) -> &PaneLayout {
        self.pane(self.tree.active_pane)
            .expect("validated active pane")
    }
    pub fn active_view_id(&self) -> Option<ViewId> {
        self.active_pane().active_view
    }
    pub fn view_ids(&self) -> impl Iterator<Item = ViewId> {
        self.panes().flat_map(|pane| pane.tabs.iter().copied())
    }
    pub fn view_count(&self) -> usize {
        self.views.len()
    }
    pub fn view_at_index(&self, mut index: usize) -> Option<(PaneId, ViewId)> {
        for pane in self.panes() {
            if index < pane.tabs.len() {
                return Some((pane.id, pane.tabs[index]));
            }
            index -= pane.tabs.len();
        }
        None
    }
    pub fn split_ratio(&self, id: SplitNodeId) -> Option<u16> {
        self.tree.root.split(id).map(|(_, ratio)| ratio)
    }
    pub fn group(&self, id: GroupId) -> Option<&TabGroup> {
        self.tree.groups.iter().find(|g| g.id == id)
    }
    pub fn view(&self, id: ViewId) -> Option<&LayoutView> {
        self.views.get(&id)
    }
    pub fn target(&self, id: ViewId) -> Option<ViewTarget> {
        self.view(id).map(|v| ViewTarget {
            view_id: id,
            document: v.document,
        })
    }
    pub fn resolve(&self, target: ViewTarget) -> Result<&LayoutView, ServiceError> {
        self.view(target.view_id)
            .filter(|v| v.document == target.document)
            .ok_or_else(|| stale("view or document lifecycle changed"))
    }
    pub fn active_group(&self) -> GroupId {
        self.active_pane()
            .active_view
            .and_then(|id| self.view(id))
            .map_or(GroupId::UNGROUPED, |v| v.group_id)
    }
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.tree.format_version != LAYOUT_FORMAT_VERSION {
            return Err(ServiceError::new(
                ErrorKind::Unsupported,
                "unsupported layout format; preserved without flattening",
            ));
        }
        if self.tree.groups.len() > MAX_CUSTOM_GROUPS + 1 || self.views.len() > MAX_SESSION_VIEWS {
            return Err(invalid("session capacity exceeded"));
        }
        let mut groups = HashMap::with_capacity(self.tree.groups.len());
        for (position, group) in self.tree.groups.iter().enumerate() {
            if groups.insert(group.id, position).is_some() {
                return Err(invalid("duplicate group identity"));
            }
            if group.id == GroupId::UNGROUPED {
                if group.name != "Ungrouped" || group.color != GroupColor::Neutral {
                    return Err(invalid("Ungrouped name and color are fixed"));
                }
            } else if normalized_name(&group.name)? != group.name {
                return Err(invalid("group name is not trimmed"));
            }
        }
        if !groups.contains_key(&GroupId::UNGROUPED) {
            return Err(invalid("Ungrouped is required"));
        }
        let mut nodes = HashSet::new();
        let mut pane_ids = HashSet::new();
        validate_nodes(&self.tree.root, 1, &mut nodes, &mut pane_ids)?;
        if !pane_ids.contains(&self.tree.active_pane) {
            return Err(invalid("missing active pane"));
        }
        let mut seen = HashSet::with_capacity(self.views.len());
        let mut documents = HashSet::with_capacity(self.views.len());
        for pane in self.panes() {
            if pane.tabs.is_empty() != pane.active_view.is_none()
                || pane.active_view.is_some_and(|id| !pane.tabs.contains(&id))
            {
                return Err(invalid("invalid pane selection or references"));
            }
            let mut previous = 0;
            for id in &pane.tabs {
                let view = self.views.get(id).ok_or_else(|| invalid("missing view"))?;
                if id.0 == 0
                    || *id != view.id
                    || !seen.insert(*id)
                    || !documents.insert(view.document.doc_id)
                {
                    return Err(invalid("duplicate or invalid view/document identity"));
                }
                let position = *groups
                    .get(&view.group_id)
                    .ok_or_else(|| invalid("missing view group"))?;
                if position < previous {
                    return Err(invalid("noncanonical group order"));
                }
                previous = position;
            }
        }
        if seen.len() != self.views.len() {
            return Err(invalid("unplaced view"));
        }
        if serde_json::to_vec(&self.tree)?.len() > MAX_LAYOUT_TREE_BYTES {
            return Err(invalid("layout tree exceeds 512 KiB"));
        }
        Ok(())
    }
    pub fn normalize(&mut self) {
        let ranks: HashMap<_, _> = self
            .tree
            .groups
            .iter()
            .enumerate()
            .map(|(i, g)| (g.id, i))
            .collect();
        let views = &self.views;
        self.tree.root.for_each_pane_mut(&mut |pane| {
            pane.tabs.sort_by_key(|id| {
                views
                    .get(id)
                    .and_then(|v| ranks.get(&v.group_id))
                    .copied()
                    .unwrap_or(usize::MAX)
            });
        });
    }
    pub fn insert_view(
        &mut self,
        pane_id: PaneId,
        view: LayoutView,
        position: Option<usize>,
        activate: bool,
    ) -> Result<(), ServiceError> {
        let pane = self
            .pane(pane_id)
            .ok_or_else(|| stale("pane no longer exists"))?;
        if self.views.len() >= MAX_SESSION_VIEWS {
            return Err(invalid(
                "session has 1024 views; close a view while keeping its stored draft first",
            ));
        }
        if self.views.contains_key(&view.id)
            || self
                .views
                .values()
                .any(|v| v.document.doc_id == view.document.doc_id)
        {
            return Err(invalid("document already has a view"));
        }
        if self.group(view.group_id).is_none() {
            return Err(stale("group no longer exists"));
        }
        let group = view.group_id;
        let id = view.id;
        let mut count = 0;
        let mut at = pane.tabs.len();
        for (i, existing) in pane.tabs.iter().enumerate() {
            if self.views[existing].group_id == group {
                if position == Some(count) {
                    at = i;
                    break;
                }
                count += 1;
                at = i + 1;
            }
        }
        self.views.insert(id, view);
        let pane = self.pane_mut(pane_id).unwrap();
        pane.tabs.insert(at, id);
        if activate || pane.active_view.is_none() {
            pane.active_view = Some(id);
        }
        if activate {
            self.tree.active_pane = pane_id;
        }
        self.normalize();
        Ok(())
    }
    pub fn remove_views(&mut self, targets: &[ViewTarget]) -> Result<(), ServiceError> {
        for target in targets {
            self.resolve(*target)?;
        }
        let removed: HashSet<_> = targets.iter().map(|t| t.view_id).collect();
        for id in &removed {
            self.views.remove(id);
        }
        self.tree.root.for_each_pane_mut(&mut |pane| {
            let old_position = pane
                .active_view
                .and_then(|id| pane.tabs.iter().position(|v| *v == id))
                .unwrap_or(0);
            pane.tabs.retain(|id| !removed.contains(id));
            if pane.active_view.is_some_and(|id| removed.contains(&id)) {
                pane.active_view = pane
                    .tabs
                    .get(old_position.min(pane.tabs.len().saturating_sub(1)))
                    .copied();
            }
        });
        Ok(())
    }
    pub fn apply_group_command(
        &mut self,
        command: GroupCommand,
    ) -> Result<GroupCommandResult, ServiceError> {
        // Validate every fallible input before changing the authoritative model.
        let mut created_group = None;
        let changed = match command {
            GroupCommand::Create {
                name,
                color,
                before,
                initial_view,
            } => {
                let name = normalized_name(&name)?;
                if self.tree.groups.len() >= MAX_CUSTOM_GROUPS + 1 {
                    return Err(invalid("at most 128 custom groups"));
                }
                let at = self.group_anchor(before)?;
                if let Some(target) = initial_view {
                    self.resolve(target)?;
                }
                let id = GroupId::new();
                self.tree.groups.insert(
                    at,
                    TabGroup {
                        id,
                        name,
                        color,
                        collapsed: false,
                    },
                );
                if let Some(target) = initial_view {
                    self.views.get_mut(&target.view_id).unwrap().group_id = id;
                }
                self.normalize();
                created_group = Some(id);
                true
            }
            GroupCommand::Rename { group, name } => {
                let name = normalized_name(&name)?;
                let g = self.custom_group_mut(group)?;
                if g.name == name {
                    false
                } else {
                    g.name = name;
                    true
                }
            }
            GroupCommand::SetColor { group, color } => {
                let g = self.custom_group_mut(group)?;
                if g.color == color {
                    false
                } else {
                    g.color = color;
                    true
                }
            }
            GroupCommand::SetCollapsed { group, collapsed } => {
                let g = self
                    .tree
                    .groups
                    .iter_mut()
                    .find(|g| g.id == group)
                    .ok_or_else(|| stale("group no longer exists"))?;
                if g.collapsed == collapsed {
                    false
                } else {
                    g.collapsed = collapsed;
                    true
                }
            }
            GroupCommand::Reorder { group, before } => {
                if before == Some(group) {
                    return Err(invalid("group cannot anchor itself"));
                }
                let at = self.group_anchor(before)?;
                let from = self
                    .tree
                    .groups
                    .iter()
                    .position(|g| g.id == group)
                    .ok_or_else(|| stale("group no longer exists"))?;
                let at = at - usize::from(from < at);
                if at == from {
                    false
                } else {
                    let g = self.tree.groups.remove(from);
                    self.tree.groups.insert(at, g);
                    self.normalize();
                    true
                }
            }
            GroupCommand::MoveView {
                target,
                group,
                before,
            } => {
                let pane = self
                    .pane_for_view(target.view_id)
                    .ok_or_else(|| stale("view is no longer placed"))?;
                self.validate_move(target, pane, pane, group, before)?;
                self.relocate_view(target.view_id, pane, pane, group, before)
            }
            GroupCommand::Remove { group } => {
                self.custom_group_mut(group)?;
                // Stable partition each leaf before changing group IDs so existing
                // ungrouped tabs precede the removed segment.
                let views = &self.views;
                self.tree.root.for_each_pane_mut(&mut |pane| {
                    pane.tabs.sort_by_key(|id| views[id].group_id == group);
                });
                for view in self.views.values_mut().filter(|v| v.group_id == group) {
                    view.group_id = GroupId::UNGROUPED;
                }
                self.tree.groups.retain(|g| g.id != group);
                self.normalize();
                true
            }
        };
        Ok(GroupCommandResult {
            changed,
            created_group,
        })
    }
    fn validate_move(
        &self,
        target: ViewTarget,
        source: PaneId,
        destination: PaneId,
        group: GroupId,
        before: Option<ViewId>,
    ) -> Result<(), ServiceError> {
        self.resolve(target)?;
        if self.pane_for_view(target.view_id) != Some(source) {
            return Err(stale("source pane changed"));
        }
        let pane = self
            .pane(destination)
            .ok_or_else(|| stale("destination pane no longer exists"))?;
        if self.group(group).is_none() {
            return Err(stale("destination group no longer exists"));
        }
        if before == Some(target.view_id) {
            return Err(invalid("view cannot anchor itself"));
        }
        if let Some(id) = before {
            if !pane.tabs.contains(&id) || self.views[&id].group_id != group {
                return Err(stale("destination anchor changed"));
            }
        }
        Ok(())
    }
    fn relocate_view(
        &mut self,
        id: ViewId,
        source: PaneId,
        destination: PaneId,
        group: GroupId,
        before: Option<ViewId>,
    ) -> bool {
        let from = self
            .pane(source)
            .unwrap()
            .tabs
            .iter()
            .position(|v| *v == id)
            .unwrap();
        let destination_pane = self.pane(destination).unwrap();
        let at = before
            .and_then(|anchor| destination_pane.tabs.iter().position(|v| *v == anchor))
            .unwrap_or_else(|| {
                destination_pane
                    .tabs
                    .iter()
                    .rposition(|v| self.views[v].group_id == group)
                    .map_or(destination_pane.tabs.len(), |i| i + 1)
            });
        let at = at - usize::from(source == destination && from < at);
        if source == destination && from == at && self.views[&id].group_id == group {
            return false;
        }
        let pane = self.pane_mut(source).unwrap();
        pane.tabs.remove(from);
        if source != destination && pane.active_view == Some(id) {
            pane.active_view = pane
                .tabs
                .get(from.min(pane.tabs.len().saturating_sub(1)))
                .copied();
        }
        self.pane_mut(destination).unwrap().tabs.insert(at, id);
        self.views.get_mut(&id).unwrap().group_id = group;
        self.normalize();
        true
    }
    pub fn apply_pane_command(
        &mut self,
        command: PaneCommand,
    ) -> Result<PaneCommandResult, ServiceError> {
        let mut result = PaneCommandResult {
            changed: false,
            created_pane: None,
            views_changed: false,
        };
        match command {
            PaneCommand::Activate { pane, view } => {
                let selected = self
                    .pane(pane)
                    .ok_or_else(|| stale("pane no longer exists"))?
                    .active_view;
                if let Some(target) = view {
                    self.resolve(target)?;
                    if self.pane_for_view(target.view_id) != Some(pane) {
                        return Err(stale("view moved to another pane"));
                    }
                }
                let next = view.map(|t| t.view_id).or(selected);
                result.changed = self.tree.active_pane != pane || selected != next;
                self.tree.active_pane = pane;
                self.pane_mut(pane).unwrap().active_view = next;
            }
            PaneCommand::Split {
                pane,
                direction,
                moving,
            } => {
                if self.pane(pane).is_none() {
                    return Err(stale("pane no longer exists"));
                }
                if self.panes().count() >= MAX_PANES {
                    return Err(invalid("at most 8 panes"));
                }
                if let Some(moving) = moving {
                    self.resolve(moving.target)?;
                    if self.pane_for_view(moving.target.view_id) != Some(moving.source) {
                        return Err(stale("source pane changed"));
                    }
                }
                let new_pane = PaneId::new();
                self.tree.root.split_pane(pane, direction, new_pane);
                if let Some(moving) = moving {
                    let group = self.views[&moving.target.view_id].group_id;
                    self.relocate_view(moving.target.view_id, moving.source, new_pane, group, None);
                    self.pane_mut(new_pane).unwrap().active_view = Some(moving.target.view_id);
                }
                self.tree.active_pane = new_pane;
                result.changed = true;
                result.created_pane = Some(new_pane);
            }
            PaneCommand::MoveView {
                target,
                source,
                destination,
                group,
                before,
            } => {
                self.validate_move(target, source, destination, group, before)?;
                result.views_changed = self.views[&target.view_id].group_id != group;
                result.changed =
                    self.relocate_view(target.view_id, source, destination, group, before);
                result.changed |= self.tree.active_pane != destination
                    || self.pane(destination).unwrap().active_view != Some(target.view_id);
                self.tree.active_pane = destination;
                self.pane_mut(destination).unwrap().active_view = Some(target.view_id);
            }
            PaneCommand::Close { pane } => {
                if self.pane(pane).is_none() {
                    return Err(stale("pane no longer exists"));
                }
                if self.panes().count() == 1 {
                    return Err(invalid("cannot close the last pane"));
                }
                let focused = self.tree.active_pane == pane;
                let (source, receiver) = self.tree.root.remove_pane(pane).unwrap();
                let destination = self.pane_mut(receiver).unwrap();
                if focused || destination.active_view.is_none() {
                    destination.active_view = source.active_view.or(destination.active_view);
                }
                destination.tabs.extend(source.tabs);
                if focused {
                    self.tree.active_pane = receiver;
                }
                self.normalize();
                result.changed = true;
            }
            PaneCommand::SetRatio {
                split,
                expected_ratio_bps,
                ratio_bps,
            } => {
                if !(MIN_SPLIT_RATIO_BPS..=MAX_SPLIT_RATIO_BPS).contains(&ratio_bps) {
                    return Err(invalid("split ratio out of range"));
                }
                let ratio = self
                    .tree
                    .root
                    .ratio_mut(split)
                    .ok_or_else(|| stale("split no longer exists"))?;
                if *ratio != expected_ratio_bps {
                    return Err(stale("split ratio changed"));
                }
                result.changed = *ratio != ratio_bps;
                *ratio = ratio_bps;
            }
            PaneCommand::Equalize => result.changed = self.tree.root.equalize().1,
        }
        Ok(result)
    }
    fn group_anchor(&self, before: Option<GroupId>) -> Result<usize, ServiceError> {
        before.map_or(Ok(self.tree.groups.len()), |id| {
            self.tree
                .groups
                .iter()
                .position(|g| g.id == id)
                .ok_or_else(|| stale("group anchor no longer exists"))
        })
    }
    fn custom_group_mut(&mut self, id: GroupId) -> Result<&mut TabGroup, ServiceError> {
        if id == GroupId::UNGROUPED {
            return Err(invalid("Ungrouped cannot be removed, renamed or recolored"));
        }
        self.tree
            .groups
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or_else(|| stale("group no longer exists"))
    }
}
fn normalized_name(name: &str) -> Result<String, ServiceError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_GROUP_NAME_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(invalid(
            "group name must be 1–128 UTF-8 bytes without control characters",
        ));
    }
    Ok(trimmed.to_owned())
}
fn invalid(message: &str) -> ServiceError {
    ServiceError::new(ErrorKind::InvalidInput, message)
}
fn stale(message: &str) -> ServiceError {
    ServiceError::new(ErrorKind::Stale, message)
}

#[derive(Clone, Copy, Debug)]
pub struct PaneMove {
    pub target: ViewTarget,
    pub source: PaneId,
}
#[derive(Clone, Copy, Debug)]
pub enum PaneCommand {
    Activate {
        pane: PaneId,
        view: Option<ViewTarget>,
    },
    Split {
        pane: PaneId,
        direction: SplitDirection,
        moving: Option<PaneMove>,
    },
    MoveView {
        target: ViewTarget,
        source: PaneId,
        destination: PaneId,
        group: GroupId,
        before: Option<ViewId>,
    },
    Close {
        pane: PaneId,
    },
    SetRatio {
        split: SplitNodeId,
        expected_ratio_bps: u16,
        ratio_bps: u16,
    },
    Equalize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneCommandResult {
    pub changed: bool,
    pub created_pane: Option<PaneId>,
    pub views_changed: bool,
}

#[derive(Clone, Debug)]
pub enum GroupCommand {
    Create {
        name: String,
        color: GroupColor,
        before: Option<GroupId>,
        initial_view: Option<ViewTarget>,
    },
    Rename {
        group: GroupId,
        name: String,
    },
    SetColor {
        group: GroupId,
        color: GroupColor,
    },
    SetCollapsed {
        group: GroupId,
        collapsed: bool,
    },
    Reorder {
        group: GroupId,
        before: Option<GroupId>,
    },
    MoveView {
        target: ViewTarget,
        group: GroupId,
        before: Option<ViewId>,
    },
    Remove {
        group: GroupId,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupCommandResult {
    pub changed: bool,
    pub created_group: Option<GroupId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionDocumentStub {
    pub doc_id: DocumentId,
    pub lifecycle_gen: LifecycleGen,
    pub durable_rev: Revision,
    pub meta: DocMeta,
    pub has_checkpoint: bool,
}
impl SessionDocumentStub {
    pub fn key(&self) -> DocumentKey {
        DocumentKey {
            doc_id: self.doc_id,
            lifecycle_gen: self.lifecycle_gen,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct SessionManifest {
    pub layout: SessionLayout,
    pub layout_revision: LayoutRevision,
    pub source_layout_revision: Option<LayoutRevision>,
    pub repaired_or_fallback: bool,
    pub documents: Vec<SessionDocumentStub>,
    pub recovery: RecoveryCatalogSummary,
    pub warnings: Vec<RecoveryWarning>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LayoutSeq(pub u64);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LayoutRevision(pub u64);
#[derive(Clone, Debug)]
pub struct DocumentMetaRecord {
    pub key: DocumentKey,
    pub meta: DocMeta,
}
#[derive(Clone, Debug)]
pub enum DocumentChange {
    Create(DocumentMetaRecord),
    Update(DocumentMetaRecord),
    Reopen(DocumentKey),
}
impl DocumentChange {
    pub fn key(&self) -> DocumentKey {
        match self {
            Self::Create(r) | Self::Update(r) => r.key,
            Self::Reopen(key) => *key,
        }
    }
}
#[derive(Clone, Debug)]
pub enum LayoutPayload {
    TreeOnly(LayoutTree),
    Structural(SessionLayout),
}
#[derive(Clone, Debug)]
pub struct SessionWrite {
    pub sequence: LayoutSeq,
    pub expected_revision: LayoutRevision,
    pub layout: LayoutPayload,
    pub document_changes: Vec<DocumentChange>,
    pub preserve_source_layout: Option<LayoutRevision>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutAck {
    pub sequence: LayoutSeq,
    pub revision: LayoutRevision,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDisposition {
    KeepRecovery,
    Discard { tombstone_gen: LifecycleGen },
}
#[derive(Clone, Debug)]
pub struct CloseEntry {
    pub target: ViewTarget,
    pub disposition: CloseDisposition,
    pub required_durable_rev: Option<Revision>,
    pub expected_format: (EncodingSpec, LineEnding),
    pub view: ViewState,
    pub expected_identity: Option<ContentIdentity>,
}
#[derive(Clone, Debug)]
pub struct CloseBatch {
    pub sequence: LayoutSeq,
    pub expected_revision: LayoutRevision,
    pub entries: Vec<CloseEntry>,
    pub after: SessionLayout,
    pub empty_pane_document: Option<DocumentMetaRecord>,
}
#[derive(Clone, Debug)]
pub struct ClosedViewRecord {
    pub close_id: u64,
    pub previous_view: LayoutView,
    pub pane_id: PaneId,
    pub position: u16,
    pub document: SessionDocumentStub,
}
#[derive(Clone, Debug)]
pub struct CloseAck {
    pub layout: LayoutAck,
    pub closed: Vec<ClosedViewRecord>,
}
#[derive(Clone, Debug)]
pub struct ClosedViewPage {
    pub entries: Vec<ClosedViewRecord>,
    pub next_before_close_id: Option<u64>,
}
#[derive(Clone, Debug)]
pub struct ReloadReplacement {
    pub sequence: LayoutSeq,
    pub expected_revision: LayoutRevision,
    pub target: ViewTarget,
    pub tombstone_gen: LifecycleGen,
    pub replacement: DocumentMetaRecord,
    pub after: SessionLayout,
}
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub enum RecoveryEntryOrigin {
    #[default]
    Document,
    LegacyOrphan,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecoveryEntryKey {
    pub origin: RecoveryEntryOrigin,
    pub rowid: i64,
    pub doc_id: Option<DocumentId>,
    pub lifecycle_gen: Option<LifecycleGen>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryCursor {
    pub origin: RecoveryEntryOrigin,
    pub after_rowid: Option<i64>,
    pub document_upper_rowid: Option<i64>,
    pub legacy_orphan_upper_rowid: Option<i64>,
}
#[derive(Clone, Copy, Debug)]
pub struct RecoveryPageRequest {
    pub cursor: Option<RecoveryCursor>,
    pub limit: u16,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoveryCatalogSummary {
    pub total_eligible: u64,
    pub admitted: u16,
    pub unplaced_checkpoints: u64,
    pub unplaced_other: u64,
    pub closed: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryEntryCategory {
    Admitted,
    Closed,
    UnplacedCheckpoint,
    UnplacedOther,
}
#[derive(Clone, Debug)]
pub struct RecoveryEntry {
    pub key: RecoveryEntryKey,
    pub category: RecoveryEntryCategory,
    pub has_checkpoint: bool,
    pub stub: Result<SessionDocumentStub, ServiceError>,
    pub last_placement: Option<ClosedViewRecord>,
}
#[derive(Clone, Debug)]
pub struct RecoveryPage {
    pub entries: Vec<RecoveryEntry>,
    pub next: Option<RecoveryCursor>,
    pub summary: RecoveryCatalogSummary,
    pub page_unreadable: u16,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn add(layout: &mut SessionLayout, group: GroupId) -> ViewTarget {
        let id = ViewId::new();
        let document = DocumentKey {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
        };
        layout
            .insert_view(
                layout.active_pane_id(),
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
    fn create(layout: &mut SessionLayout, name: &str) -> GroupId {
        layout
            .apply_group_command(GroupCommand::Create {
                name: name.into(),
                color: GroupColor::Blue,
                before: None,
                initial_view: None,
            })
            .unwrap()
            .created_group
            .unwrap()
    }
    #[test]
    fn stable_normalization_remove_and_collapsed_activation() {
        let mut l = SessionLayout::default();
        let a = add(&mut l, GroupId::UNGROUPED);
        let g = create(&mut l, "  notes  ");
        let b = add(&mut l, g);
        let c = add(&mut l, g);
        l.apply_group_command(GroupCommand::Reorder {
            group: g,
            before: Some(GroupId::UNGROUPED),
        })
        .unwrap();
        assert_eq!(l.active_pane().tabs, vec![b.view_id, c.view_id, a.view_id]);
        l.apply_group_command(GroupCommand::SetCollapsed {
            group: g,
            collapsed: true,
        })
        .unwrap();
        assert_eq!(l.active_pane().active_view, Some(c.view_id));
        l.apply_group_command(GroupCommand::Remove { group: g })
            .unwrap();
        assert_eq!(l.active_pane().tabs, vec![a.view_id, b.view_id, c.view_id]);
        l.validate().unwrap();
    }
    #[test]
    fn limits_and_stale_commands_are_atomic() {
        let mut l = SessionLayout::default();
        for _ in 0..MAX_CUSTOM_GROUPS {
            create(&mut l, &"é".repeat(64));
        }
        let before = l.clone();
        assert!(
            l.apply_group_command(GroupCommand::Create {
                name: "extra".into(),
                color: GroupColor::Red,
                before: None,
                initial_view: None
            })
            .is_err()
        );
        assert_eq!(l, before);
        assert!(
            l.apply_group_command(GroupCommand::Rename {
                group: l.tree.groups[1].id,
                name: "é".repeat(65)
            })
            .is_err()
        );
        assert_eq!(l, before);
        for _ in 0..MAX_SESSION_VIEWS {
            add(&mut l, GroupId::UNGROUPED);
        }
        let before = l.clone();
        assert!(
            l.insert_view(
                l.active_pane_id(),
                LayoutView {
                    id: ViewId::new(),
                    document: DocumentKey {
                        doc_id: DocumentId::new(),
                        lifecycle_gen: LifecycleGen(1)
                    },
                    group_id: GroupId::UNGROUPED,
                    view: ViewState::default()
                },
                None,
                true
            )
            .is_err()
        );
        assert_eq!(l, before);
        l.validate().unwrap();
    }
    #[test]
    fn stale_lifecycle_self_anchor_and_noop_drop() {
        let mut l = SessionLayout::default();
        let a = add(&mut l, GroupId::UNGROUPED);
        let b = add(&mut l, GroupId::UNGROUPED);
        assert!(
            !l.apply_group_command(GroupCommand::MoveView {
                target: a,
                group: GroupId::UNGROUPED,
                before: Some(b.view_id)
            })
            .unwrap()
            .changed
        );
        let before = l.clone();
        assert_eq!(
            l.apply_group_command(GroupCommand::MoveView {
                target: a,
                group: GroupId::UNGROUPED,
                before: Some(a.view_id)
            })
            .unwrap_err()
            .kind,
            ErrorKind::InvalidInput
        );
        let stale = ViewTarget {
            document: DocumentKey {
                lifecycle_gen: LifecycleGen(2),
                ..a.document
            },
            ..a
        };
        assert_eq!(
            l.apply_group_command(GroupCommand::MoveView {
                target: stale,
                group: GroupId::UNGROUPED,
                before: None
            })
            .unwrap_err()
            .kind,
            ErrorKind::Stale
        );
        assert_eq!(l, before);
    }
}
