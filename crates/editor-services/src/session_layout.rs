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
pub const LAYOUT_FORMAT_VERSION: u32 = 1;

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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutRoot {
    Pane {
        node_id: SplitNodeId,
        pane: PaneLayout,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutTree {
    pub format_version: u32,
    pub groups: Vec<TabGroup>,
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
        Self {
            tree: LayoutTree {
                format_version: LAYOUT_FORMAT_VERSION,
                groups: vec![TabGroup {
                    id: GroupId::UNGROUPED,
                    name: "Ungrouped".into(),
                    color: GroupColor::Neutral,
                    collapsed: false,
                }],
                root: LayoutRoot::Pane {
                    node_id: SplitNodeId::new(),
                    pane: PaneLayout {
                        id: PaneId::new(),
                        tabs: Vec::new(),
                        active_view: None,
                    },
                },
            },
            views: HashMap::new(),
        }
    }
}
impl SessionLayout {
    pub fn pane(&self) -> &PaneLayout {
        let LayoutRoot::Pane { pane, .. } = &self.tree.root;
        pane
    }
    pub fn pane_mut(&mut self) -> &mut PaneLayout {
        let LayoutRoot::Pane { pane, .. } = &mut self.tree.root;
        pane
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
        self.pane()
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
        let pane = self.pane();
        if pane.id.0 == 0
            || matches!(
                self.tree.root,
                LayoutRoot::Pane {
                    node_id: SplitNodeId(0),
                    ..
                }
            )
        {
            return Err(invalid("zero pane identity"));
        }
        if pane.tabs.len() != self.views.len()
            || (pane.tabs.is_empty() != pane.active_view.is_none())
            || pane.active_view.is_some_and(|id| !pane.tabs.contains(&id))
        {
            return Err(invalid("invalid pane selection or references"));
        }
        let mut seen = HashSet::with_capacity(self.views.len());
        let mut documents = HashSet::with_capacity(self.views.len());
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
        let LayoutRoot::Pane { pane, .. } = &mut self.tree.root;
        pane.tabs.sort_by_key(|id| {
            views
                .get(id)
                .and_then(|v| ranks.get(&v.group_id))
                .copied()
                .unwrap_or(usize::MAX)
        });
    }
    pub fn insert_view(
        &mut self,
        view: LayoutView,
        position: Option<usize>,
        activate: bool,
    ) -> Result<(), ServiceError> {
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
        let segment: Vec<_> = self
            .pane()
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, id)| self.views[id].group_id == group)
            .map(|(i, _)| i)
            .collect();
        let offset = position.unwrap_or(segment.len()).min(segment.len());
        let at = segment
            .get(offset)
            .copied()
            .unwrap_or_else(|| segment.last().map_or(self.pane().tabs.len(), |i| i + 1));
        self.views.insert(id, view);
        self.pane_mut().tabs.insert(at, id);
        if activate || self.pane().active_view.is_none() {
            self.pane_mut().active_view = Some(id);
        }
        self.normalize();
        Ok(())
    }
    pub fn remove_views(&mut self, targets: &[ViewTarget]) -> Result<(), ServiceError> {
        for target in targets {
            self.resolve(*target)?;
        }
        let removed: HashSet<_> = targets.iter().map(|t| t.view_id).collect();
        let old_position = self
            .pane()
            .active_view
            .and_then(|id| self.pane().tabs.iter().position(|v| *v == id))
            .unwrap_or(0);
        for id in &removed {
            self.views.remove(id);
        }
        self.pane_mut().tabs.retain(|id| !removed.contains(id));
        if self
            .pane()
            .active_view
            .is_some_and(|id| removed.contains(&id))
        {
            self.pane_mut().active_view = self
                .pane()
                .tabs
                .get(old_position.min(self.pane().tabs.len().saturating_sub(1)))
                .copied();
        }
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
                let old_group = self.resolve(target)?.group_id;
                if self.group(group).is_none() {
                    return Err(stale("destination group no longer exists"));
                }
                if before == Some(target.view_id) {
                    return Err(invalid("view cannot anchor itself"));
                }
                if let Some(id) = before {
                    if self.view(id).is_none_or(|v| v.group_id != group) {
                        return Err(stale("destination anchor changed"));
                    }
                }
                let from = self
                    .pane()
                    .tabs
                    .iter()
                    .position(|id| *id == target.view_id)
                    .unwrap();
                let at = before
                    .and_then(|id| self.pane().tabs.iter().position(|v| *v == id))
                    .unwrap_or_else(|| {
                        self.pane()
                            .tabs
                            .iter()
                            .rposition(|id| self.views[id].group_id == group)
                            .map_or(self.pane().tabs.len(), |i| i + 1)
                    });
                let at = at - usize::from(from < at);
                if old_group == group && at == from {
                    false
                } else {
                    self.pane_mut().tabs.remove(from);
                    self.pane_mut().tabs.insert(at, target.view_id);
                    self.views.get_mut(&target.view_id).unwrap().group_id = group;
                    self.normalize();
                    true
                }
            }
            GroupCommand::Remove { group } => {
                self.custom_group_mut(group)?;
                // Existing ungrouped views must precede the removed segment regardless of group order.
                let members: Vec<_> = self
                    .pane()
                    .tabs
                    .iter()
                    .copied()
                    .filter(|id| self.views[id].group_id == group)
                    .collect();
                let members_set: HashSet<_> = members.iter().copied().collect();
                self.pane_mut().tabs.retain(|id| !members_set.contains(id));
                for id in &members {
                    self.views.get_mut(id).unwrap().group_id = GroupId::UNGROUPED;
                }
                self.pane_mut().tabs.extend(members);
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
        assert_eq!(l.pane().tabs, vec![b.view_id, c.view_id, a.view_id]);
        l.apply_group_command(GroupCommand::SetCollapsed {
            group: g,
            collapsed: true,
        })
        .unwrap();
        assert_eq!(l.pane().active_view, Some(c.view_id));
        l.apply_group_command(GroupCommand::Remove { group: g })
            .unwrap();
        assert_eq!(l.pane().tabs, vec![a.view_id, b.view_id, c.view_id]);
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
