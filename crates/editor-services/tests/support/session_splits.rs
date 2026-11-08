use super::*;
use rusqlite::{Connection, OpenFlags};

fn split(layout: &mut SessionLayout, pane: PaneId, direction: SplitDirection) -> PaneId {
    layout
        .apply_pane_command(PaneCommand::Split {
            pane,
            direction,
            moving: None,
        })
        .unwrap()
        .created_pane
        .unwrap()
}
fn add_in(layout: &mut SessionLayout, pane: PaneId, group: GroupId) -> ViewTarget {
    let target = ViewTarget {
        view_id: ViewId::new(),
        document: document_key(DocumentId::new()),
    };
    layout
        .insert_view(
            pane,
            LayoutView {
                id: target.view_id,
                document: target.document,
                group_id: group,
                view: ViewState {
                    cursor_byte: 7,
                    scroll_byte: 3,
                },
            },
            None,
            true,
        )
        .unwrap();
    target
}
fn group(layout: &mut SessionLayout) -> GroupId {
    layout
        .apply_group_command(GroupCommand::Create {
            name: "Alpha".into(),
            color: GroupColor::Blue,
            before: None,
            initial_view: None,
        })
        .unwrap()
        .created_group
        .unwrap()
}

#[test]
fn pane_moves_preserve_document_state_and_repair_each_selection() {
    let mut layout = SessionLayout::default();
    let a = layout.active_pane_id();
    let alpha = group(&mut layout);
    let x = add_in(&mut layout, a, GroupId::UNGROUPED);
    let y = add_in(&mut layout, a, GroupId::UNGROUPED);
    let b = split(&mut layout, a, SplitDirection::Right);
    let z = add_in(&mut layout, b, alpha);
    let original = layout.views[&y.view_id].clone();
    let result = layout
        .apply_pane_command(PaneCommand::MoveView {
            target: y,
            source: a,
            destination: b,
            group: alpha,
            before: Some(z.view_id),
        })
        .unwrap();
    assert!(result.changed && result.views_changed);
    assert_eq!(layout.views.len(), 3);
    assert_eq!(layout.views[&y.view_id].document, original.document);
    assert_eq!(layout.views[&y.view_id].view, original.view);
    assert_eq!(layout.pane(a).unwrap().active_view, Some(x.view_id));
    assert_eq!(layout.pane(b).unwrap().tabs, vec![y.view_id, z.view_id]);
    assert_eq!(layout.active_view_id(), Some(y.view_id));
    assert_eq!(layout.view_at_index(0), Some((a, x.view_id)));
    assert_eq!(layout.view_at_index(2), Some((b, z.view_id)));
    assert_eq!(layout.view_at_index(3), None);
    layout.validate().unwrap();
    layout
        .apply_pane_command(PaneCommand::Split {
            pane: b,
            direction: SplitDirection::Up,
            moving: Some(PaneMove {
                target: x,
                source: a,
            }),
        })
        .unwrap();
    assert!(layout.pane(a).unwrap().tabs.is_empty());
    assert_eq!(layout.pane(a).unwrap().active_view, None);
    assert_eq!(layout.views.len(), 3);
    layout.validate().unwrap();
}

#[test]
fn close_merges_group_segments_into_adjacent_boundary_without_closing_documents() {
    let mut layout = SessionLayout::default();
    let a = layout.active_pane_id();
    let alpha = group(&mut layout);
    let a0 = add_in(&mut layout, a, GroupId::UNGROUPED);
    let a1 = add_in(&mut layout, a, alpha);
    let b = split(&mut layout, a, SplitDirection::Right);
    let b0 = add_in(&mut layout, b, GroupId::UNGROUPED);
    let b1 = add_in(&mut layout, b, alpha);
    let c = split(&mut layout, b, SplitDirection::Down);
    let c0 = add_in(&mut layout, c, GroupId::UNGROUPED);
    layout
        .apply_pane_command(PaneCommand::Activate {
            pane: a,
            view: Some(a1),
        })
        .unwrap();
    let views = layout.views.clone();
    let sibling = match &layout.tree.root {
        LayoutRoot::Split { second, .. } => second.as_ref().clone(),
        _ => unreachable!(),
    };
    layout
        .apply_pane_command(PaneCommand::Close { pane: a })
        .unwrap();
    assert_eq!(layout.views, views);
    assert_eq!(layout.active_pane_id(), b);
    assert_eq!(
        layout.pane(b).unwrap().tabs,
        vec![b0.view_id, a0.view_id, b1.view_id, a1.view_id]
    );
    assert_eq!(layout.active_view_id(), Some(a1.view_id));
    let node_id = |root: &LayoutRoot| match root {
        LayoutRoot::Pane { node_id, .. } | LayoutRoot::Split { node_id, .. } => *node_id,
    };
    assert_eq!(node_id(&layout.tree.root), node_id(&sibling));
    layout
        .apply_pane_command(PaneCommand::Close { pane: c })
        .unwrap();
    assert_eq!(layout.active_view_id(), Some(a1.view_id));
    assert_eq!(
        layout.pane(b).unwrap().tabs,
        vec![b0.view_id, a0.view_id, c0.view_id, b1.view_id, a1.view_id]
    );
    let before = layout.clone();
    assert!(
        layout
            .apply_pane_command(PaneCommand::Close { pane: b })
            .is_err()
    );
    assert_eq!(layout, before);
    layout.validate().unwrap();
}

#[test]
fn closing_second_child_uses_last_leaf_and_empty_focus_keeps_receiver_selection() {
    let mut layout = SessionLayout::default();
    let left = layout.active_pane_id();
    let right = split(&mut layout, left, SplitDirection::Right);
    let lower = split(&mut layout, left, SplitDirection::Down);
    let selected = add_in(&mut layout, lower, GroupId::UNGROUPED);
    layout
        .apply_pane_command(PaneCommand::Activate {
            pane: right,
            view: None,
        })
        .unwrap();
    layout
        .apply_pane_command(PaneCommand::Close { pane: right })
        .unwrap();
    assert_eq!(layout.active_pane_id(), lower);
    assert_eq!(layout.active_view_id(), Some(selected.view_id));
    assert!(layout.pane(left).unwrap().tabs.is_empty());
    layout.validate().unwrap();
}

#[test]
fn tree_caps_stale_edge_moves_ratios_and_cross_pane_anchors_are_atomic() {
    let mut layout = SessionLayout::default();
    let first = layout.active_pane_id();
    let x = add_in(&mut layout, first, GroupId::UNGROUPED);
    let second = split(&mut layout, first, SplitDirection::Left);
    let y = add_in(&mut layout, second, GroupId::UNGROUPED);
    let before = layout.clone();
    let stale_target = ViewTarget {
        document: DocumentKey {
            lifecycle_gen: LifecycleGen(2),
            ..x.document
        },
        ..x
    };
    for command in [
        PaneCommand::Split {
            pane: second,
            direction: SplitDirection::Down,
            moving: Some(PaneMove {
                target: stale_target,
                source: first,
            }),
        },
        PaneCommand::Split {
            pane: second,
            direction: SplitDirection::Down,
            moving: Some(PaneMove {
                target: x,
                source: second,
            }),
        },
        PaneCommand::MoveView {
            target: x,
            source: first,
            destination: second,
            group: GroupId::UNGROUPED,
            before: Some(x.view_id),
        },
        PaneCommand::Activate {
            pane: first,
            view: Some(y),
        },
        PaneCommand::Close {
            pane: PaneId::new(),
        },
    ] {
        assert!(layout.apply_pane_command(command).is_err());
        assert_eq!(layout, before);
    }
    assert!(
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: x,
                group: GroupId::UNGROUPED,
                before: Some(y.view_id)
            })
            .is_err()
    );
    assert_eq!(layout, before);
    let root = match layout.tree.root {
        LayoutRoot::Split { node_id, .. } => node_id,
        _ => unreachable!(),
    };
    for (expected, ratio) in [(5001, 6000), (5000, 499), (5000, 9501)] {
        assert!(
            layout
                .apply_pane_command(PaneCommand::SetRatio {
                    split: root,
                    expected_ratio_bps: expected,
                    ratio_bps: ratio
                })
                .is_err()
        );
        assert_eq!(layout, before);
    }
    split(&mut layout, second, SplitDirection::Up);
    layout.apply_pane_command(PaneCommand::Equalize).unwrap();
    assert_eq!(layout.split_ratio(root), Some(6667));
    while layout.panes().count() < MAX_PANES {
        let pane = layout.active_pane_id();
        split(&mut layout, pane, SplitDirection::Down);
    }
    layout.validate().unwrap();
    let before = layout.clone();
    assert!(
        layout
            .apply_pane_command(PaneCommand::Split {
                pane: first,
                direction: SplitDirection::Right,
                moving: None
            })
            .is_err()
    );
    assert_eq!(layout, before);
    let mut invalid = before.clone();
    invalid.tree.active_pane = PaneId::new();
    assert!(invalid.validate().is_err());
    let mut invalid = before.clone();
    invalid.pane_mut(first).unwrap().tabs.push(y.view_id);
    assert!(invalid.validate().is_err());
}

#[test]
fn empty_split_restart_preserves_tree_and_recovers_unplaced_crash_head_in_active_leaf() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let mut layout = receive(service.request_manifest().unwrap()).layout;
    let first = layout.active_pane_id();
    let second = split(&mut layout, first, SplitDirection::Right);
    group(&mut layout);
    let root = match layout.tree.root {
        LayoutRoot::Split { node_id, .. } => node_id,
        _ => unreachable!(),
    };
    layout
        .apply_pane_command(PaneCommand::SetRatio {
            split: root,
            expected_ratio_bps: 5000,
            ratio_bps: 6200,
        })
        .unwrap();
    persist_test_layout(&service, &layout, vec![]);
    receive(service.shutdown().unwrap());
    drop(service);
    let service = RecoveryHandle::start(&db).unwrap();
    let empty = receive(service.request_manifest().unwrap());
    assert_eq!(empty.layout, layout);
    assert!(!empty.repaired_or_fallback);
    let id = DocumentId::new();
    admit(|| service.try_commit(snapshot(id, 1, "unplaced crash 中文🙂")));
    receive(admit(|| service.flush()));
    receive(service.shutdown().unwrap());
    drop(service);
    let service = RecoveryHandle::start(&db).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert!(manifest.repaired_or_fallback);
    assert_eq!(manifest.documents.len(), 1);
    assert_eq!(manifest.documents[0].doc_id, id);
    assert_eq!(manifest.layout.active_pane_id(), second);
    let target = manifest
        .layout
        .target(manifest.layout.active_view_id().unwrap())
        .unwrap();
    assert_eq!(manifest.layout.pane_for_view(target.view_id), Some(second));
    let mut stripped = manifest.layout;
    stripped.remove_views(&[target]).unwrap();
    assert_eq!(stripped, layout);
    assert_eq!(
        receive(
            service
                .request_hydrate(target.document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "unplaced crash 中文🙂"
    );
    receive(service.shutdown().unwrap());
}

#[test]
fn format_one_upgrades_in_memory_without_rewriting_raw_snapshot_until_normal_write() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let layout = receive(service.request_manifest().unwrap()).layout;
    receive(service.shutdown().unwrap());
    drop(service);
    let conn = Connection::open(&db).unwrap();
    let mut legacy = serde_json::to_value(&layout.tree).unwrap();
    legacy["format_version"] = 1.into();
    legacy.as_object_mut().unwrap().remove("active_pane");
    let raw = serde_json::to_vec(&legacy).unwrap();
    conn.execute("UPDATE layout_states SET format_version=1,tree=?1", [&raw])
        .unwrap();
    drop(conn);
    let service = RecoveryHandle::start(&db).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout, layout);
    assert_eq!(manifest.layout.tree.format_version, 2);
    assert!(!manifest.repaired_or_fallback);
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let (version, stored): (u32, Vec<u8>) = conn.query_row("SELECT format_version,tree FROM layout_states WHERE id=(SELECT layout_id FROM layout_heads WHERE slot='current')", [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!((version, stored), (1, raw.clone()));
    persist_test_layout(&service, &manifest.layout, vec![]);
    let version: u32 = conn.query_row("SELECT format_version FROM layout_states WHERE id=(SELECT layout_id FROM layout_heads WHERE slot='current')", [], |row| row.get(0)).unwrap();
    assert_eq!(version, 2);
    let old: Vec<u8> = conn.query_row("SELECT tree FROM layout_states WHERE id=(SELECT layout_id FROM layout_heads WHERE slot='last_valid')", [], |row| row.get(0)).unwrap();
    assert_eq!(old, raw);
    receive(service.shutdown().unwrap());
}

#[test]
fn cross_pane_group_move_and_closed_placement_survive_restart() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let mut layout = receive(service.request_manifest().unwrap()).layout;
    let a = layout.active_pane_id();
    let b = split(&mut layout, a, SplitDirection::Right);
    let alpha = group(&mut layout);
    let x = add_in(&mut layout, a, GroupId::UNGROUPED);
    let y = add_in(&mut layout, b, alpha);
    let changes = [x, y]
        .into_iter()
        .map(|target| {
            DocumentChange::Create(DocumentMetaRecord {
                key: target.document,
                meta: DocMeta::default(),
            })
        })
        .collect();
    persist_test_layout(&service, &layout, changes);
    let moved = layout
        .apply_pane_command(PaneCommand::MoveView {
            target: x,
            source: a,
            destination: b,
            group: alpha,
            before: Some(y.view_id),
        })
        .unwrap();
    assert!(moved.views_changed);
    persist_test_layout(&service, &layout, vec![]);
    receive(service.shutdown().unwrap());
    drop(service);
    let service = RecoveryHandle::start(&db).unwrap();
    assert_eq!(receive(service.request_manifest().unwrap()).layout, layout);
    layout
        .apply_pane_command(PaneCommand::Activate {
            pane: a,
            view: None,
        })
        .unwrap();
    persist_test_layout(&service, &layout, vec![]);
    let closed = close_test_view(&service, y, CloseDisposition::KeepRecovery, None);
    assert_eq!(closed.closed[0].pane_id, b);
    assert_eq!(closed.closed[0].position, 1);
    receive(service.shutdown().unwrap());
}

#[test]
fn global_capacity_trim_repairs_all_leaf_selections_without_flattening() {
    let mut layout = SessionLayout::default();
    for direction in [
        SplitDirection::Right,
        SplitDirection::Down,
        SplitDirection::Left,
    ] {
        let pane = layout.active_pane_id();
        split(&mut layout, pane, direction);
    }
    let panes = layout.panes().map(|pane| pane.id).collect::<Vec<_>>();
    for index in 0..MAX_SESSION_VIEWS {
        add_in(&mut layout, panes[index % panes.len()], GroupId::UNGROUPED);
    }
    let removed = layout
        .view_ids()
        .skip(257)
        .filter_map(|id| layout.target(id))
        .collect::<Vec<_>>();
    layout.remove_views(&removed).unwrap();
    assert_eq!(
        layout.panes().map(|pane| pane.id).collect::<Vec<_>>(),
        panes
    );
    assert_eq!(layout.view_count(), 257);
    assert_eq!(layout.pane(panes[0]).unwrap().tabs.len(), 256);
    assert_eq!(layout.pane(panes[1]).unwrap().tabs.len(), 1);
    assert!(layout.pane(panes[2]).unwrap().tabs.is_empty());
    assert!(layout.pane(panes[3]).unwrap().tabs.is_empty());
    layout.validate().unwrap();
}

#[test]
fn sql_json_version_disagreement_preserves_current_and_uses_last_valid() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let original = receive(service.request_manifest().unwrap()).layout;
    let mut changed = original.clone();
    group(&mut changed);
    let revision = persist_test_layout(&service, &changed, vec![]);
    receive(service.shutdown().unwrap());
    drop(service);
    let mut wrong = serde_json::to_value(&changed.tree).unwrap();
    wrong["format_version"] = 1.into();
    wrong.as_object_mut().unwrap().remove("active_pane");
    let raw = serde_json::to_vec(&wrong).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE layout_states SET tree=?1 WHERE id=?2",
        rusqlite::params![raw, revision.0 as i64],
    )
    .unwrap();
    drop(conn);
    let service = RecoveryHandle::start(&db).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert!(manifest.repaired_or_fallback);
    assert!(!manifest.warnings.is_empty());
    assert_eq!(manifest.layout, original);
    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let retained: Vec<u8> = conn
        .query_row(
            "SELECT tree FROM layout_states WHERE id=?1",
            [revision.0 as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, raw);
    receive(service.shutdown().unwrap());
}

#[test]
fn duplicate_leaf_and_node_ids_and_invalid_ratio_are_rejected_on_restore_validation() {
    let mut layout = SessionLayout::default();
    let pane = layout.active_pane_id();
    split(&mut layout, pane, SplitDirection::Right);
    let mut duplicate_pane = layout.clone();
    if let LayoutRoot::Split { first, second, .. } = &mut duplicate_pane.tree.root {
        if let (LayoutRoot::Pane { pane: a, .. }, LayoutRoot::Pane { pane: b, .. }) =
            (first.as_ref(), second.as_mut())
        {
            b.id = a.id;
        }
    }
    assert!(duplicate_pane.validate().is_err());
    let mut duplicate_node = layout.clone();
    if let LayoutRoot::Split { node_id, first, .. } = &mut duplicate_node.tree.root {
        if let LayoutRoot::Pane { node_id: child, .. } = first.as_mut() {
            *child = *node_id;
        }
    }
    assert!(duplicate_node.validate().is_err());
    if let LayoutRoot::Split { ratio_bps, .. } = &mut layout.tree.root {
        *ratio_bps = 0;
    }
    assert!(layout.validate().is_err());
}

#[test]
fn bounded_future_sibling_and_groups_override_corruption_before_opening_writer() {
    for slot in ["current", "last_valid"] {
        for case in [
            "sibling_field",
            "sibling_variant",
            "sibling_axis",
            "groups",
            "missing_root_groups",
        ] {
            let dir = TempDir::new().unwrap();
            let db = dir.path().join("db");
            let service = RecoveryHandle::start(&db).unwrap();
            let layout = receive(service.request_manifest().unwrap()).layout;
            persist_test_layout(&service, &layout, vec![]);
            receive(service.shutdown().unwrap());
            drop(service);
            let mut tree = serde_json::to_value(&layout.tree).unwrap();
            let mut second = tree["root"].clone();
            match case {
                "sibling_field" => second["Pane"]["future_field"] = true.into(),
                "sibling_variant" => second = serde_json::json!({"FutureNode": {}}),
                "sibling_axis" => {
                    second = serde_json::json!({"Split": {
                        "node_id": SplitNodeId::new().key(), "axis": "FutureAxis", "ratio_bps": 5000
                    }})
                }
                _ => tree["groups"][0]["future_field"] = true.into(),
            }
            // A missing first child is corrupt, but the bounded second child
            // and group list must still receive read-only future-format inspection.
            tree["root"] = serde_json::json!({"Split": {
                "node_id": SplitNodeId::new().key(), "axis": "Horizontal",
                "ratio_bps": 5000, "second": second
            }});
            if case == "missing_root_groups" {
                tree.as_object_mut().unwrap().remove("root");
            }
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE layout_states SET tree=?1 WHERE id=(SELECT layout_id FROM layout_heads WHERE slot=?2)",
                rusqlite::params![serde_json::to_vec(&tree).unwrap(), slot]).unwrap();
            let before = super::session_groups::disk_images(&db);
            let error = match RecoveryHandle::start(&db) {
                Ok(_) => panic!("opened a writer for {slot}/{case}"),
                Err(error) => error,
            };
            assert_eq!(error.kind, ErrorKind::Unsupported, "{slot}/{case}");
            assert_eq!(
                super::session_groups::disk_images(&db),
                before,
                "{slot}/{case}"
            );
        }
    }
}
