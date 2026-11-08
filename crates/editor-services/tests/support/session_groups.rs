use super::*;
use rusqlite::{Connection, params};
use std::collections::HashSet;

fn response<T>(reply: Reply<T>) -> Result<T, ServiceError> {
    reply
        .recv_timeout(Duration::from_secs(30))
        .expect("worker reply")
}
fn start_error(path: &Path, faults: FaultInjector) -> ServiceError {
    match RecoveryHandle::start_with_faults(path, faults) {
        Ok(_) => panic!("unexpected writable recovery worker"),
        Err(e) => e,
    }
}
fn bytes_if_present(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("{e}"),
    }
}
pub(super) fn disk_images(path: &Path) -> (Vec<u8>, Option<Vec<u8>>) {
    (
        fs::read(path).unwrap(),
        bytes_if_present(&path.with_file_name(format!(
            "{}-wal",
            path.file_name().unwrap().to_str().unwrap()
        ))),
    )
}
fn entry(target: ViewTarget, required: Option<Revision>) -> CloseEntry {
    CloseEntry {
        target,
        disposition: CloseDisposition::KeepRecovery,
        required_durable_rev: required,
        expected_format: (EncodingSpec::Utf8, LineEnding::Lf),
        view: ViewState::default(),
        expected_identity: None,
    }
}
fn batch(layout: &SessionLayout, revision: LayoutRevision, entries: Vec<CloseEntry>) -> CloseBatch {
    let mut after = layout.clone();
    after
        .remove_views(&entries.iter().map(|entry| entry.target).collect::<Vec<_>>())
        .unwrap();
    CloseBatch {
        sequence: LayoutSeq(10),
        expected_revision: revision,
        entries,
        after,
        empty_pane_document: None,
    }
}
fn metadata_documents(
    service: &RecoveryHandle,
    count: usize,
) -> (SessionLayout, Vec<ViewTarget>, LayoutRevision) {
    let mut layout = SessionLayout::default();
    let mut targets = Vec::new();
    let mut changes = Vec::new();
    for n in 0..count {
        let key = document_key(DocumentId::new());
        targets.push(add_layout_view(
            &mut layout,
            key,
            ViewState {
                cursor_byte: n as u64,
                scroll_byte: n as u64,
            },
            n == 0,
        ));
        changes.push(DocumentChange::Create(DocumentMetaRecord {
            key,
            meta: DocMeta {
                title: format!("draft {n}"),
                ..DocMeta::default()
            },
        }));
    }
    let revision = persist_test_layout(service, &layout, changes);
    (layout, targets, revision)
}
fn legacy_database(path: &Path, count: usize) {
    let mut conn = Connection::open(path).unwrap();
    conn.execute_batch("CREATE TABLE documents(id TEXT PRIMARY KEY,gen INTEGER NOT NULL,state INTEGER NOT NULL,meta TEXT NOT NULL,saved_rev INTEGER NOT NULL DEFAULT -1,saved_meta TEXT); CREATE TABLE recovery_heads(doc TEXT PRIMARY KEY,checkpoint_rev INTEGER NOT NULL,durable_rev INTEGER NOT NULL,byte_len INTEGER NOT NULL,checkpoint_hash BLOB NOT NULL,delta_bytes INTEGER NOT NULL DEFAULT 0,delta_rows INTEGER NOT NULL DEFAULT 0); CREATE TABLE checkpoint_chunks(doc TEXT NOT NULL,rev INTEGER NOT NULL,ordinal INTEGER NOT NULL,content BLOB NOT NULL,checksum BLOB NOT NULL,PRIMARY KEY(doc,rev,ordinal)); CREATE TABLE edit_transactions(doc TEXT NOT NULL,base_rev INTEGER NOT NULL,new_rev INTEGER NOT NULL,deltas BLOB NOT NULL,checksum BLOB NOT NULL,PRIMARY KEY(doc,new_rev)); CREATE TABLE save_intents(doc TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE windows_views(doc TEXT PRIMARY KEY,position INTEGER,view TEXT); CREATE TABLE session(key TEXT PRIMARY KEY,value TEXT); PRAGMA user_version=1;").unwrap();
    let tx = conn.transaction().unwrap();
    for n in 0..count {
        let id = format!("{:032x}", n + 1);
        let meta = serde_json::to_string(&DocMeta {
            title: format!("legacy {n}"),
            ..DocMeta::default()
        })
        .unwrap();
        tx.execute(
            "INSERT INTO documents(id,gen,state,meta) VALUES(?1,0,0,?2)",
            params![id, meta],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO windows_views VALUES(?1,?2,?3)",
            params![
                id,
                n as i64,
                serde_json::to_string(&ViewState {
                    cursor_byte: n as u64,
                    scroll_byte: 0
                })
                .unwrap()
            ],
        )
        .unwrap();
    }
    tx.execute(
        "INSERT INTO session VALUES('active',?1)",
        [format!("{:032x}", count.max(1))],
    )
    .unwrap();
    tx.commit().unwrap();
}

#[test]
fn premature_layout_is_unqueued_and_stale_cas_is_atomic() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let empty = SessionLayout::default();
    let premature = service.write_session(SessionWrite {
        sequence: LayoutSeq(1),
        expected_revision: LayoutRevision(0),
        layout: LayoutPayload::Structural(empty),
        document_changes: Vec::new(),
        preserve_source_layout: None,
    });
    assert_eq!(premature.err().unwrap().kind, ErrorKind::Stale);
    let (mut layout, targets, base) = metadata_documents(&service, 2);
    let created = layout
        .apply_group_command(GroupCommand::Create {
            name: "Work".into(),
            color: GroupColor::Blue,
            before: None,
            initial_view: Some(targets[0]),
        })
        .unwrap();
    let before = receive(service.request_manifest().unwrap());
    let stale = SessionWrite {
        sequence: LayoutSeq(2),
        expected_revision: LayoutRevision(base.0 - 1),
        layout: LayoutPayload::Structural(layout),
        document_changes: Vec::new(),
        preserve_source_layout: None,
    };
    assert_eq!(
        response(service.write_session(stale).unwrap())
            .unwrap_err()
            .kind,
        ErrorKind::Stale
    );
    let after = receive(service.request_manifest().unwrap());
    assert_eq!(after.layout, before.layout);
    assert!(created.changed);
    assert_eq!(after.layout_revision, base);
    receive(admit(|| service.shutdown()));
}
#[test]
fn metadata_session_admits_exactly_1024_without_hydration() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let (mut layout, targets, _) = metadata_documents(&service, MAX_SESSION_VIEWS);
    let before = layout.clone();
    let too_many = layout.insert_view(
        layout.active_pane_id(),
        LayoutView {
            id: ViewId::new(),
            document: document_key(DocumentId::new()),
            group_id: GroupId::UNGROUPED,
            view: ViewState::default(),
        },
        None,
        false,
    );
    assert!(too_many.is_err());
    assert_eq!(layout, before);
    receive(admit(|| service.shutdown()));
    drop(service);
    let service = RecoveryHandle::start(&db).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout, layout);
    assert_eq!(manifest.documents.len(), 1024);
    assert_eq!(
        manifest.layout.active_pane().active_view,
        Some(targets[0].view_id)
    );
    assert!(manifest.documents.iter().all(|stub| !stub.has_checkpoint));
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM checkpoint_chunks", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn tree_only_group_metadata_never_rewrites_document_or_reference_rows() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let (mut layout, _, base) = metadata_documents(&service, 100);
    let conn = Connection::open(&db).unwrap();
    let set_before:i64=conn.query_row("SELECT s.view_set FROM layout_states s JOIN layout_heads h ON h.layout_id=s.id WHERE h.slot='current'",[],|r|r.get(0)).unwrap();
    conn.execute_batch("CREATE TRIGGER forbid_document_update BEFORE UPDATE ON documents BEGIN SELECT RAISE(ABORT,'tree wrote document'); END; CREATE TRIGGER forbid_view_insert BEFORE INSERT ON layout_views BEGIN SELECT RAISE(ABORT,'tree rewrote view references'); END;").unwrap();
    let group = layout
        .apply_group_command(GroupCommand::Create {
            name: "Empty group".into(),
            color: GroupColor::Purple,
            before: None,
            initial_view: None,
        })
        .unwrap()
        .created_group
        .unwrap();
    layout
        .apply_group_command(GroupCommand::Rename {
            group,
            name: "Renamed".into(),
        })
        .unwrap();
    layout
        .apply_group_command(GroupCommand::SetCollapsed {
            group,
            collapsed: true,
        })
        .unwrap();
    layout
        .apply_group_command(GroupCommand::SetColor {
            group,
            color: GroupColor::Pink,
        })
        .unwrap();
    let ack = receive(
        service
            .write_session(SessionWrite {
                sequence: LayoutSeq(2),
                expected_revision: base,
                layout: LayoutPayload::TreeOnly(layout.tree.clone()),
                document_changes: Vec::new(),
                preserve_source_layout: None,
            })
            .unwrap(),
    );
    assert!(ack.revision > base);
    let restored = receive(service.request_manifest().unwrap());
    assert_eq!(restored.layout, layout);
    let set_after:i64=conn.query_row("SELECT s.view_set FROM layout_states s JOIN layout_heads h ON h.layout_id=s.id WHERE h.slot='current'",[],|r|r.get(0)).unwrap();
    assert_eq!(set_before, set_after);
    receive(admit(|| service.shutdown()));
}
#[test]
fn legacy_overflow_is_archived_and_every_record_remains_keyset_browseable() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    legacy_database(&db, 1100);
    let original = Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT view FROM windows_views WHERE position=1099",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    let service = RecoveryHandle::start(&db).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.documents.len(), 1024);
    assert_eq!(manifest.recovery.total_eligible, 1100);
    assert_eq!(manifest.recovery.unplaced_other, 76);
    assert!(
        manifest
            .documents
            .iter()
            .all(|stub| stub.lifecycle_gen == LifecycleGen(0))
    );
    let mut cursor = None;
    let mut seen = HashSet::new();
    loop {
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest { cursor, limit: 64 })
                .unwrap(),
        );
        assert!(page.entries.len() <= 64);
        for entry in page.entries {
            assert!(seen.insert(entry.key.rowid));
            assert!(entry.stub.is_ok());
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1100);
    assert_eq!(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 65
            })
            .err()
            .unwrap()
            .kind,
        ErrorKind::InvalidInput
    );
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM legacy_v1_windows_views", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1100
    );
    assert_eq!(
        conn.query_row(
            "SELECT view FROM legacy_v1_windows_views WHERE position=1099",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        original
    );
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn migration_failure_rolls_back_schema_archives_version_and_payloads() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    legacy_database(&db, 3);
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "INSERT INTO checkpoint_chunks VALUES(?1,0,0,x'ff0088',x'ab')",
        [format!("{:032x}", 1)],
    )
    .unwrap();
    drop(conn);
    let before = disk_images(&db);
    let faults = FaultInjector::default();
    faults.arm(FaultPoint::RecoveryBeforeCommit);
    assert_eq!(start_error(&db, faults).kind, ErrorKind::Injected);
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT count(*) FROM windows_views", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='legacy_v1_windows_views'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row("SELECT content FROM checkpoint_chunks", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![255, 0, 136]
    );
    assert_eq!(
        conn.pragma_query_value(None, "journal_mode", |r| r.get::<_, String>(0))
            .unwrap(),
        "delete"
    );
    drop(conn);
    assert_eq!(disk_images(&db), before);
}
#[test]
fn future_wal_schema_and_nonempty_unversioned_database_are_untouched() {
    for future in [false, true] {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("db");
        let conn = Connection::open(&db).unwrap();
        if future {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE future_payload(value BLOB); INSERT INTO future_payload VALUES(x'abcd'); PRAGMA user_version=3;").unwrap();
        } else {
            conn.execute_batch("CREATE TABLE unknown_payload(value); INSERT INTO unknown_payload VALUES('do not replace');").unwrap();
        }
        let before = disk_images(&db);
        assert_eq!(
            start_error(&db, FaultInjector::default()).kind,
            ErrorKind::Unsupported
        );
        assert_eq!(disk_images(&db), before);
    }
}
#[test]
fn future_layout_variant_refuses_writer_without_flattening() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    receive(admit(|| service.shutdown()));
    drop(service);
    let conn = Connection::open(&db).unwrap();
    let tree = serde_json::json!({"format_version":1,"groups":[],"root":{"Split":{"children":[]}}});
    conn.execute("UPDATE layout_states SET tree=?1 WHERE id=(SELECT layout_id FROM layout_heads WHERE slot='current')",[tree.to_string().into_bytes()]).unwrap();
    let before = disk_images(&db);
    assert_eq!(
        start_error(&db, FaultInjector::default()).kind,
        ErrorKind::Unsupported
    );
    assert_eq!(disk_images(&db), before);
}
#[test]
fn corrupt_document_preserves_99_grouped_views_and_original_reference_set() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let (mut layout, targets, _) = metadata_documents(&service, 100);
    let group = layout
        .apply_group_command(GroupCommand::Create {
            name: "Working set".into(),
            color: GroupColor::Teal,
            before: None,
            initial_view: None,
        })
        .unwrap()
        .created_group
        .unwrap();
    for target in &targets {
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: *target,
                group,
                before: None,
            })
            .unwrap();
    }
    layout
        .apply_group_command(GroupCommand::SetCollapsed {
            group,
            collapsed: true,
        })
        .unwrap();
    let base = persist_test_layout(&service, &layout, Vec::new());
    let conn = Connection::open(&db).unwrap();
    let raw: Vec<u8> = conn
        .query_row(
            "SELECT tree FROM layout_states WHERE id=?1",
            [base.0 as i64],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE documents SET meta=?1 WHERE id=?2",
        params![
            "界".repeat(30_000),
            format!("{:032x}", targets[50].document.doc_id.0)
        ],
    )
    .unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout.views.len(), 99);
    assert_eq!(manifest.layout.tree.groups, layout.tree.groups);
    assert!(!manifest.layout.views.contains_key(&targets[50].view_id));
    assert_eq!(
        conn.query_row(
            "SELECT tree FROM layout_states WHERE id=?1",
            [base.0 as i64],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        raw
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM layout_views WHERE view_set=(SELECT view_set FROM layout_states WHERE id=?1)",[base.0 as i64],|r|r.get::<_,i64>(0)).unwrap(),100);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM preserved_layouts WHERE layout_id=?1",
            [base.0 as i64],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    let mut cursor = None;
    let mut found = false;
    loop {
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest { cursor, limit: 64 })
                .unwrap(),
        );
        for entry in page.entries {
            if entry.key.doc_id == Some(targets[50].document.doc_id) {
                assert_eq!(entry.stub.unwrap_err().kind, ErrorKind::Corrupt);
                found = true;
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(found);
    receive(admit(|| service.shutdown()));
}
#[test]
fn last_valid_never_resurrects_deliberately_closed_documents() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let (layout, targets, base) = metadata_documents(&service, 3);
    let ack = receive(
        service
            .close_views(batch(&layout, base, vec![entry(targets[0], None)]))
            .unwrap(),
    );
    let conn = Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE layout_states SET tree=x'ff00' WHERE id=?1",
        [ack.layout.revision.0 as i64],
    )
    .unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.documents.len(), 2);
    assert!(
        !manifest
            .documents
            .iter()
            .any(|stub| stub.doc_id == targets[0].document.doc_id)
    );
    assert_eq!(
        receive(service.request_recently_closed(None, 64).unwrap())
            .entries
            .len(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT tree FROM layout_states WHERE id=?1",
            [ack.layout.revision.0 as i64],
            |r| r.get::<_, Vec<u8>>(0)
        )
        .unwrap(),
        vec![255, 0]
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn legacy_checkpoint_zero_is_distinct_from_no_expected_head() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let id = DocumentId::new();
    let mut job = snapshot(id, 0, "legacy checkpoint at zero");
    job.lifecycle_gen = LifecycleGen(0);
    admit(|| service.try_commit(job.clone()));
    receive(admit(|| service.flush()));
    let key = DocumentKey {
        doc_id: id,
        lifecycle_gen: LifecycleGen(0),
    };
    let mut layout = SessionLayout::default();
    let target = add_layout_view(&mut layout, key, ViewState::default(), true);
    let base = persist_test_layout(
        &service,
        &layout,
        vec![DocumentChange::Create(DocumentMetaRecord {
            key,
            meta: job.meta,
        })],
    );
    assert_eq!(
        response(
            service
                .close_views(batch(&layout, base, vec![entry(target, None)]))
                .unwrap()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Stale
    );
    assert_eq!(receive(service.request_manifest().unwrap()).layout, layout);
    receive(
        service
            .close_views(batch(&layout, base, vec![entry(target, Some(Revision(0)))]))
            .unwrap(),
    );
    let draft = receive(service.request_hydrate(key, CancelToken::new()).unwrap());
    assert_eq!(draft.rope, "legacy checkpoint at zero");
    assert_eq!(draft.durable_rev, Revision(0));
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM recovery_heads", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn thousand_lazy_protected_drafts_close_atomically_without_hydration() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let faults = FaultInjector::default();
    let service = RecoveryHandle::start_with_faults(&db, faults.clone()).unwrap();
    let (mut layout, targets, _) = metadata_documents(&service, 1000);
    let group = layout
        .apply_group_command(GroupCommand::Create {
            name: "Lazy protected".into(),
            color: GroupColor::Orange,
            before: None,
            initial_view: None,
        })
        .unwrap()
        .created_group
        .unwrap();
    for (n, target) in targets.iter().enumerate() {
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: *target,
                group,
                before: None,
            })
            .unwrap();
        let job = snapshot(target.document.doc_id, 1, &format!("unopened draft {n}"));
        admit(|| service.try_commit(job.clone()));
        if n % 32 == 31 {
            receive(admit(|| service.flush()));
            service.events().try_iter().for_each(drop);
        }
    }
    receive(admit(|| service.flush()));
    let base = persist_test_layout(&service, &layout, Vec::new());
    let request = batch(
        &layout,
        base,
        targets
            .iter()
            .map(|target| entry(*target, Some(Revision(1))))
            .collect(),
    );
    faults.arm(FaultPoint::RecoveryBeforeCommit);
    assert_eq!(
        response(service.close_views(request.clone()).unwrap())
            .unwrap_err()
            .kind,
        ErrorKind::Injected
    );
    assert_eq!(receive(service.request_manifest().unwrap()).layout, layout);
    let ack = receive(service.close_views(request).unwrap());
    assert_eq!(ack.closed.len(), 1000);
    let manifest = receive(service.request_manifest().unwrap());
    assert!(manifest.layout.views.is_empty());
    assert!(manifest.layout.group(group).is_some());
    assert_eq!(manifest.recovery.closed, 1000);
    let mut before = None;
    let mut seen = HashSet::new();
    loop {
        let page = receive(service.request_recently_closed(before, 64).unwrap());
        assert!(page.entries.len() <= 64);
        for closed in page.entries {
            assert!(seen.insert(closed.document.doc_id));
            assert!(closed.document.has_checkpoint);
        }
        before = page.next_before_close_id;
        if before.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1000);
    assert_eq!(
        receive(
            service
                .request_hydrate(targets[999].document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "unopened draft 999"
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn reload_replaces_only_one_binding_with_no_checkpoint_and_rolls_back_failure() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let path = dir.path().join("source.txt");
    fs::write(&path, "disk clean 中文").unwrap();
    let faults = FaultInjector::default();
    let service = RecoveryHandle::start_with_faults(&db, faults.clone()).unwrap();
    let (layout, targets, base) = metadata_documents(&service, 2);
    let old = snapshot(targets[0].document.doc_id, 1, "unsaved old draft");
    admit(|| service.try_commit(old.clone()));
    receive(admit(|| service.flush()));
    let opened = open_document(
        DocumentId::new(),
        &path,
        &PagingService::default(),
        OpenOptions::default(),
        &CancelToken::new(),
    )
    .unwrap();
    let (identity, fingerprint, encoding, line_ending) = match opened {
        OpenOutcome::Editable {
            identity,
            fingerprint,
            encoding,
            line_ending,
            ..
        } => (identity, fingerprint, encoding, line_ending),
        _ => panic!("ordinary disk source"),
    };
    let key = document_key(DocumentId::new());
    let meta = DocMeta {
        path: Some(path),
        origin_fingerprint: Some(fingerprint),
        saved_identity: Some(identity),
        saved_encoding: Some(encoding),
        saved_line_ending: Some(line_ending),
        encoding,
        line_ending,
        ..DocMeta::default()
    };
    let mut after = layout.clone();
    after.views.get_mut(&targets[0].view_id).unwrap().document = key;
    let replacement = ReloadReplacement {
        sequence: LayoutSeq(4),
        expected_revision: base,
        target: targets[0],
        tombstone_gen: LifecycleGen(2),
        replacement: DocumentMetaRecord { key, meta },
        after: after.clone(),
    };
    faults.arm(FaultPoint::RecoveryBeforeCommit);
    assert_eq!(
        response(service.replace_view_document(replacement.clone()).unwrap())
            .unwrap_err()
            .kind,
        ErrorKind::Injected
    );
    assert_eq!(receive(service.request_manifest().unwrap()).layout, layout);
    assert_eq!(
        receive(
            service
                .request_hydrate(targets[0].document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "unsaved old draft"
    );
    receive(service.replace_view_document(replacement).unwrap());
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout, after);
    assert_eq!(manifest.layout.target(targets[1].view_id), Some(targets[1]));
    assert!(
        !manifest
            .documents
            .iter()
            .find(|stub| stub.key() == key)
            .unwrap()
            .has_checkpoint
    );
    assert!(
        response(
            service
                .request_hydrate(targets[0].document, CancelToken::new())
                .unwrap()
        )
        .is_err()
    );
    let conn = Connection::open(&db).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM checkpoint_chunks WHERE doc=?1",
            [format!("{:032x}", key.doc_id.0)],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    receive(admit(|| service.shutdown()));
}
#[cfg(target_os = "macos")]
#[test]
fn saved_oversize_close_requires_current_full_identity_and_exact_published_proof() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("db");
    let service = RecoveryHandle::start(&db).unwrap();
    let (layout, targets, base) = metadata_documents(&service, 1);
    let target = targets[0];
    let old = snapshot(target.document.doc_id, 1, "obsolete small checkpoint");
    admit(|| service.try_commit(old.clone()));
    receive(admit(|| service.flush()));
    let rope = Rope::from_str(&"x".repeat(65 * 1024 * 1024));
    let identity = ContentIdentity::of_rope(&rope);
    let request = SaveRequest {
        doc_id: target.document.doc_id,
        lifecycle_gen: target.document.lifecycle_gen,
        captured_rev: Revision(2),
        rope,
        target_path: dir.path().join("large.txt"),
        encoding: EncodingSpec::Utf8,
        line_ending: LineEnding::Lf,
        expected_fingerprint: None,
        force_overwrite: false,
    };
    assert!(matches!(
        save_document(&request, Some(&service), &FaultInjector::default()),
        SaveOutcome::Saved { .. }
    ));
    let mut close = entry(target, Some(Revision(2)));
    close.expected_identity = Some(ContentIdentity {
        byte_len: identity.byte_len,
        hash128: identity.hash128 ^ 1,
    });
    assert!(
        response(
            service
                .close_views(batch(&layout, base, vec![close.clone()]))
                .unwrap()
        )
        .is_err()
    );
    close.expected_identity = Some(identity);
    close.expected_format.0 = EncodingSpec::Utf8Bom;
    assert!(
        response(
            service
                .close_views(batch(&layout, base, vec![close.clone()]))
                .unwrap()
        )
        .is_err()
    );
    close.expected_format.0 = EncodingSpec::Utf8;
    let conn = Connection::open(&db).unwrap();
    let saved: String = conn
        .query_row(
            "SELECT saved_meta FROM documents WHERE id=?1",
            [format!("{:032x}", target.document.doc_id.0)],
            |r| r.get(0),
        )
        .unwrap();
    let mut proof: SaveIntent = serde_json::from_str(&saved).unwrap();
    proof.lifecycle_gen = LifecycleGen(2);
    conn.execute(
        "UPDATE documents SET saved_meta=?1 WHERE id=?2",
        params![
            serde_json::to_string(&proof).unwrap(),
            format!("{:032x}", target.document.doc_id.0)
        ],
    )
    .unwrap();
    assert!(
        response(
            service
                .close_views(batch(&layout, base, vec![close.clone()]))
                .unwrap()
        )
        .is_err()
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(target.document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "obsolete small checkpoint"
    );
    conn.execute(
        "UPDATE documents SET saved_meta=?1 WHERE id=?2",
        params![saved, format!("{:032x}", target.document.doc_id.0)],
    )
    .unwrap();
    receive(
        service
            .close_views(batch(&layout, base, vec![close]))
            .unwrap(),
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM recovery_heads WHERE doc=?1",
            [format!("{:032x}", target.document.doc_id.0)],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        receive(service.request_recently_closed(None, 64).unwrap()).entries[0]
            .document
            .meta
            .saved_identity,
        Some(identity)
    );
    receive(admit(|| service.shutdown()));
}
#[cfg(target_os = "macos")]
#[test]
fn export_copy_publishes_without_changing_source_association_or_save_intents() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let (layout, targets, _) = metadata_documents(&service, 1);
    let job = snapshot(targets[0].document.doc_id, 1, "export 中文🙂");
    admit(|| service.try_commit(job.clone()));
    receive(admit(|| service.flush()));
    let draft = receive(
        service
            .request_hydrate(targets[0].document, CancelToken::new())
            .unwrap(),
    );
    let path = dir.path().join("copy.txt");
    let request = SaveRequest {
        doc_id: draft.doc_id,
        lifecycle_gen: draft.lifecycle_gen,
        captured_rev: draft.durable_rev,
        rope: draft.rope,
        target_path: path.clone(),
        encoding: EncodingSpec::Utf8,
        line_ending: LineEnding::Lf,
        expected_fingerprint: None,
        force_overwrite: false,
    };
    assert!(matches!(
        save_document_copy(&request, &FaultInjector::default()),
        SaveOutcome::Saved { .. }
    ));
    assert_eq!(fs::read_to_string(path).unwrap(), "export 中文🙂");
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout, layout);
    assert!(manifest.documents[0].meta.path.is_none());
    assert!(manifest.documents[0].meta.saved_identity.is_none());
    assert!(receive(service.request_save_intents().unwrap()).is_empty());
    receive(admit(|| service.shutdown()));
}

#[test]
fn durable_overflow_pages_reopen_and_platform_export_preserve_all_heads() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("overflow.sqlite3");
    let service = RecoveryHandle::start(&database).unwrap();
    let (layout, targets, _) = metadata_documents(&service, MAX_SESSION_VIEWS);
    let mut keys = targets
        .iter()
        .map(|target| target.document)
        .collect::<Vec<_>>();
    keys.extend((0..76).map(|_| document_key(DocumentId::new())));
    for (index, key) in keys.iter().enumerate() {
        let job = snapshot(key.doc_id, 1, &format!("durable overflow {index} 中文🙂\n"));
        admit(|| service.try_commit(job.clone()));
        if index % 32 == 31 {
            receive(admit(|| service.flush()));
            service.events().try_iter().for_each(drop);
        }
    }
    receive(admit(|| service.shutdown()));
    drop(service);
    let service = RecoveryHandle::start(&database).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout, layout);
    assert_eq!(manifest.documents.len(), 1024);
    assert_eq!(manifest.recovery.total_eligible, 1100);
    assert_eq!(manifest.recovery.admitted, 1024);
    assert_eq!(manifest.recovery.unplaced_checkpoints, 76);
    assert_eq!(manifest.recovery.unplaced_other, 0);
    let mut cursor = None;
    let mut seen = HashSet::new();
    let mut overflow = None;
    loop {
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest { cursor, limit: 64 })
                .unwrap(),
        );
        assert!(page.entries.len() <= 64);
        assert_eq!(page.page_unreadable, 0);
        assert_eq!(page.summary.total_eligible, 1100);
        for record in page.entries {
            assert!(record.has_checkpoint);
            let stub = record.stub.unwrap();
            assert!(seen.insert(stub.doc_id));
            if overflow.is_none() && record.category == RecoveryEntryCategory::UnplacedCheckpoint {
                overflow = Some(stub);
            }
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 1100);
    let overflow = overflow.unwrap();
    #[cfg(target_os = "macos")]
    {
        let draft = receive(
            service
                .request_hydrate(overflow.key(), CancelToken::new())
                .unwrap(),
        );
        let expected = draft.rope.to_string();
        let destination = directory.path().join("overflow-copy.txt");
        let request = SaveRequest {
            doc_id: draft.doc_id,
            lifecycle_gen: draft.lifecycle_gen,
            captured_rev: draft.durable_rev,
            rope: draft.rope,
            target_path: destination.clone(),
            encoding: draft.meta.encoding,
            line_ending: draft.meta.line_ending,
            expected_fingerprint: None,
            force_overwrite: false,
        };
        assert!(matches!(
            save_document_copy(&request, &FaultInjector::default()),
            SaveOutcome::Saved { .. }
        ));
        assert_eq!(fs::read_to_string(destination).unwrap(), expected);
        assert_eq!(receive(service.request_manifest().unwrap()).layout, layout);
        let after = receive(
            service
                .request_hydrate(overflow.key(), CancelToken::new())
                .unwrap(),
        );
        assert_eq!(
            serde_json::to_value(after.meta).unwrap(),
            serde_json::to_value(&overflow.meta).unwrap()
        );
        assert!(receive(service.request_save_intents().unwrap()).is_empty());
    }
    let closed = receive(
        service
            .close_views(batch(
                &layout,
                manifest.layout_revision,
                vec![entry(targets[0], Some(Revision(1)))],
            ))
            .unwrap(),
    );
    let mut reopened = layout.clone();
    reopened.remove_views(&[targets[0]]).unwrap();
    let admitted = add_layout_view(&mut reopened, overflow.key(), ViewState::default(), true);
    receive(
        service
            .write_session(SessionWrite {
                sequence: LayoutSeq(99),
                expected_revision: closed.layout.revision,
                layout: LayoutPayload::Structural(reopened.clone()),
                document_changes: Vec::new(),
                preserve_source_layout: None,
            })
            .unwrap(),
    );
    let after = receive(service.request_manifest().unwrap());
    assert_eq!(after.layout, reopened);
    assert_eq!(after.layout.target(admitted.view_id), Some(admitted));
    assert_eq!(after.recovery.total_eligible, 1100);
    assert_eq!(after.recovery.admitted, 1024);
    assert_eq!(after.recovery.closed, 1);
    assert_eq!(after.recovery.unplaced_checkpoints, 75);
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM recovery_heads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1100
    );
    receive(admit(|| service.shutdown()));
}

fn staged_disk_metadata(path: &Path) -> DocMeta {
    match open_document(
        DocumentId::new(),
        path,
        &PagingService::default(),
        OpenOptions::default(),
        &CancelToken::new(),
    )
    .unwrap()
    {
        OpenOutcome::Editable {
            identity,
            fingerprint,
            encoding,
            line_ending,
            ..
        } => DocMeta {
            path: Some(path.to_path_buf()),
            origin_fingerprint: Some(fingerprint),
            saved_identity: Some(identity),
            saved_encoding: Some(encoding),
            saved_line_ending: Some(line_ending),
            encoding,
            line_ending,
            ..DocMeta::default()
        },
        _ => panic!("small UTF-8 source must be editable"),
    }
}

#[test]
fn missing_reload_source_requires_initial_lifecycle_and_rolls_back_its_tombstone() {
    for old_generation in [1, 2] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("reload.sqlite3");
        let source = directory.path().join("source.txt");
        fs::write(&source, "verified disk 中文\n").unwrap();
        let faults = FaultInjector::default();
        let service = RecoveryHandle::start_with_faults(&database, faults.clone()).unwrap();
        let (mut layout, targets, base) = metadata_documents(&service, 2);
        let mut target = targets[0];
        let connection = Connection::open(&database).unwrap();
        // Only the external fixture connection may manufacture a dangling reference.
        connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        if old_generation != 1 {
            target.document.lifecycle_gen = LifecycleGen(old_generation);
            layout.views.get_mut(&target.view_id).unwrap().document = target.document;
            connection
                .execute(
                    "UPDATE layout_views SET gen=?1 WHERE view_id=?2",
                    params![old_generation as i64, target.view_id.key()],
                )
                .unwrap();
        }
        connection
            .execute(
                "DELETE FROM documents WHERE id=?1",
                [format!("{:032x}", target.document.doc_id.0)],
            )
            .unwrap();
        let fresh = document_key(DocumentId::new());
        let mut after = layout.clone();
        after.views.get_mut(&target.view_id).unwrap().document = fresh;
        let request = ReloadReplacement {
            sequence: LayoutSeq(44),
            expected_revision: base,
            target,
            tombstone_gen: LifecycleGen(old_generation + 1),
            replacement: DocumentMetaRecord {
                key: fresh,
                meta: staged_disk_metadata(&source),
            },
            after: after.clone(),
        };
        if old_generation == 1 {
            faults.arm(FaultPoint::RecoveryBeforeCommit);
            assert_eq!(
                response(service.replace_view_document(request.clone()).unwrap())
                    .unwrap_err()
                    .kind,
                ErrorKind::Injected
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM documents WHERE id IN (?1,?2)",
                        params![
                            format!("{:032x}", target.document.doc_id.0),
                            format!("{:032x}", fresh.doc_id.0)
                        ],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            receive(service.replace_view_document(request).unwrap());
            assert_eq!(
                connection
                    .query_row(
                        "SELECT gen,state FROM documents WHERE id=?1",
                        [format!("{:032x}", target.document.doc_id.0)],
                        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                    )
                    .unwrap(),
                (2, 2)
            );
            let manifest = receive(service.request_manifest().unwrap());
            assert_eq!(manifest.layout, after);
            assert!(
                !manifest
                    .documents
                    .iter()
                    .find(|stub| stub.key() == fresh)
                    .unwrap()
                    .has_checkpoint
            );
            assert!(
                response(
                    service
                        .request_hydrate(target.document, CancelToken::new())
                        .unwrap()
                )
                .is_err()
            );
        } else {
            assert_eq!(
                response(service.replace_view_document(request).unwrap())
                    .unwrap_err()
                    .kind,
                ErrorKind::Stale
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM documents WHERE id IN (?1,?2)",
                        params![
                            format!("{:032x}", target.document.doc_id.0),
                            format!("{:032x}", fresh.doc_id.0)
                        ],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT layout_id FROM layout_heads WHERE slot='current'",
                        [],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                i64::try_from(base.0).unwrap()
            );
        }
        receive(admit(|| service.shutdown()));
    }
}

#[test]
fn lost_close_reply_has_authoritative_after_image_and_failed_close_has_pre_image() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("lost-close.sqlite3");
    let faults = FaultInjector::default();
    let service = RecoveryHandle::start_with_faults(&database, faults.clone()).unwrap();
    let (layout, targets, base) = metadata_documents(&service, 2);
    for target in &targets {
        let job = snapshot(target.document.doc_id, 1, "retained despite lost reply");
        admit(|| service.try_commit(job.clone()));
    }
    receive(admit(|| service.flush()));
    let request = batch(&layout, base, vec![entry(targets[0], Some(Revision(1)))]);
    faults.arm(FaultPoint::RecoveryBeforeCommit);
    drop(service.close_views(request.clone()).unwrap());
    let barrier = receive(service.request_manifest().unwrap());
    assert_eq!(barrier.layout_revision, base);
    let (before, closed) = inspect_recovery_session(&database).unwrap();
    assert_eq!(before.layout, layout);
    assert!(closed.entries.is_empty());
    drop(service.close_views(request.clone()).unwrap());
    let barrier = receive(service.request_manifest().unwrap());
    assert!(barrier.layout_revision > base);
    let (after, closed) = inspect_recovery_session(&database).unwrap();
    assert_eq!(after.layout, request.after);
    assert_eq!(closed.entries.len(), 1);
    assert_eq!(closed.entries[0].document.key(), targets[0].document);
    assert_eq!(
        read_recovery_draft(&database, targets[0].document, &CancelToken::new())
            .unwrap()
            .rope,
        "retained despite lost reply"
    );
    assert_eq!(
        response(
            service
                .write_session(SessionWrite {
                    sequence: LayoutSeq(77),
                    expected_revision: after.layout_revision,
                    layout: LayoutPayload::Structural(layout),
                    document_changes: Vec::new(),
                    preserve_source_layout: None
                })
                .unwrap()
        )
        .unwrap_err()
        .kind,
        ErrorKind::Stale
    );
    receive(admit(|| service.shutdown()));
}

#[test]
fn startup_and_lost_shutdown_inspections_are_read_only_and_require_complete_proofs() {
    let directory = TempDir::new().unwrap();
    let legacy = directory.path().join("legacy.sqlite3");
    legacy_database(&legacy, 1);
    let before = disk_images(&legacy);
    assert!(matches!(
        inspect_recovery_startup(&legacy).unwrap(),
        StartupRecoveryState::PreImage
    ));
    assert_eq!(disk_images(&legacy), before);
    let connection = Connection::open(&legacy).unwrap();
    connection
        .execute_batch("CREATE TABLE layout_states(value);")
        .unwrap();
    drop(connection);
    assert_eq!(
        inspect_recovery_startup(&legacy).unwrap_err().kind,
        ErrorKind::Corrupt
    );
    let database = directory.path().join("shutdown.sqlite3");
    let service = RecoveryHandle::start(&database).unwrap();
    let (layout, targets, _) = metadata_documents(&service, 1);
    let required = [(targets[0].document, Revision(1))];
    assert!(!inspect_recovery_shutdown(&database, &layout, &required).unwrap());
    let job = snapshot(targets[0].document.doc_id, 1, "shutdown reply can be lost");
    admit(|| service.try_commit(job.clone()));
    drop(admit(|| service.shutdown()));
    match service.request_manifest() {
        Ok(reply) => assert!(reply.recv_timeout(Duration::from_secs(10)).is_err()),
        Err(error) => assert_eq!(error.kind, ErrorKind::Storage),
    }
    assert!(inspect_recovery_shutdown(&database, &layout, &required).unwrap());
    assert!(
        !inspect_recovery_shutdown(&database, &layout, &[(targets[0].document, Revision(2))])
            .unwrap()
    );
    let mut wrong = layout.clone();
    wrong
        .views
        .get_mut(&targets[0].view_id)
        .unwrap()
        .document
        .doc_id = DocumentId::new();
    assert_eq!(
        inspect_recovery_shutdown(&database, &wrong, &required)
            .unwrap_err()
            .kind,
        ErrorKind::Stale
    );
    match inspect_recovery_startup(&database).unwrap() {
        StartupRecoveryState::AfterImage { manifest, .. } => assert_eq!(manifest.layout, layout),
        StartupRecoveryState::PreImage => panic!("committed v2 schema was misclassified"),
    }
    assert_eq!(
        read_recovery_draft(&database, targets[0].document, &CancelToken::new())
            .unwrap()
            .rope,
        "shutdown reply can be lost"
    );
}

#[test]
fn malformed_document_states_generations_and_large_ids_are_keyed_unreadable_entries() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("malformed.sqlite3");
    let service = RecoveryHandle::start(&database).unwrap();
    let (mut layout, targets, _) = metadata_documents(&service, 4);
    let group = layout
        .apply_group_command(GroupCommand::Create {
            name: "Healthy group".into(),
            color: GroupColor::Green,
            before: None,
            initial_view: None,
        })
        .unwrap()
        .created_group
        .unwrap();
    for target in &targets {
        layout
            .apply_group_command(GroupCommand::MoveView {
                target: *target,
                group,
                before: None,
            })
            .unwrap();
        let job = snapshot(target.document.doc_id, 1, "preserved checkpoint");
        admit(|| service.try_commit(job.clone()));
    }
    receive(admit(|| service.flush()));
    persist_test_layout(&service, &layout, Vec::new());
    let connection = Connection::open(&database).unwrap();
    // Model corrupt persisted identifiers without changing the worker's FK policy.
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
        .execute(
            "UPDATE documents SET state=9 WHERE id=?1",
            [format!("{:032x}", targets[1].document.doc_id.0)],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE documents SET gen=-1 WHERE id=?1",
            [format!("{:032x}", targets[2].document.doc_id.0)],
        )
        .unwrap();
    let huge_id = "界".repeat(70_000);
    let old_id = format!("{:032x}", targets[3].document.doc_id.0);
    connection
        .execute(
            "UPDATE documents SET id=?1 WHERE id=?2",
            params![huge_id, old_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE recovery_heads SET doc=?1 WHERE doc=?2",
            params![huge_id, old_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE checkpoint_chunks SET doc=?1 WHERE doc=?2",
            params![huge_id, old_id],
        )
        .unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout.views.len(), 1);
    assert_eq!(manifest.layout.tree.groups, layout.tree.groups);
    assert_eq!(manifest.layout.target(targets[0].view_id), Some(targets[0]));
    let page = receive(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 64,
            })
            .unwrap(),
    );
    assert_eq!(page.entries.len(), 4);
    assert_eq!(page.page_unreadable, 3);
    assert_eq!(page.summary.total_eligible, 4);
    assert_eq!(page.summary.admitted, 1);
    assert_eq!(page.summary.unplaced_checkpoints, 3);
    assert_eq!(
        page.entries
            .iter()
            .filter(|record| record.category == RecoveryEntryCategory::Admitted)
            .count(),
        1
    );
    assert!(page.entries.iter().any(|record| record.key.doc_id.is_none()
        && record.stub.as_ref().unwrap_err().kind == ErrorKind::Corrupt));
    for index in [1, 2] {
        assert_eq!(
            response(
                service
                    .request_hydrate(targets[index].document, CancelToken::new())
                    .unwrap()
            )
            .unwrap_err()
            .kind,
            ErrorKind::Corrupt
        );
    }
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM recovery_heads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        4
    );
    receive(admit(|| service.shutdown()));
}

#[test]
fn malformed_closed_page_advances_by_scanned_keys_without_hiding_older_records() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("closed-page.sqlite3");
    let service = RecoveryHandle::start(&database).unwrap();
    let (layout, targets, base) = metadata_documents(&service, 65);
    receive(
        service
            .close_views(batch(
                &layout,
                base,
                targets
                    .into_iter()
                    .map(|target| entry(target, None))
                    .collect(),
            ))
            .unwrap(),
    );
    let connection = Connection::open(&database).unwrap();
    connection.execute("UPDATE closed_views SET view_state=zeroblob(70000) WHERE close_id>(SELECT MIN(close_id) FROM closed_views)",[]).unwrap();
    let first = receive(service.request_recently_closed(None, 64).unwrap());
    assert!(first.entries.is_empty());
    assert!(first.next_before_close_id.is_some());
    let second = receive(
        service
            .request_recently_closed(first.next_before_close_id, 64)
            .unwrap(),
    );
    assert_eq!(second.entries.len(), 1);
    assert!(second.next_before_close_id.is_none());
    assert_eq!(
        receive(
            service
                .request_recovery_page(RecoveryPageRequest {
                    cursor: None,
                    limit: 64
                })
                .unwrap()
        )
        .summary
        .closed,
        65
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM closed_views", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        65
    );
    receive(admit(|| service.shutdown()));
}

#[test]
fn catalog_pages_cover_signed_document_and_orphan_domains_without_decoding_unbounded_rows() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("catalog-domains.sqlite3");
    legacy_database(&database, 5);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute("UPDATE windows_views SET rowid=100+position", [])
        .unwrap();
    for (number, rowid) in [(1, i64::MIN), (2, -3), (3, 0), (4, 7), (5, 8)] {
        connection
            .execute(
                "UPDATE documents SET rowid=?1 WHERE id=?2",
                params![rowid, format!("{number:032x}")],
            )
            .unwrap();
    }
    connection
        .execute(
            "UPDATE documents SET gen='invalid-generation' WHERE rowid=-3",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE documents SET state='invalid-state' WHERE rowid=0",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE documents SET id=CAST(x'ff' AS TEXT) WHERE rowid=7",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE windows_views SET doc=CAST(x'ff' AS TEXT) WHERE doc=?1",
            [format!("{:032x}", 4)],
        )
        .unwrap();
    connection
        .execute("UPDATE documents SET state=2 WHERE rowid=8", [])
        .unwrap();
    let orphan_id = format!("{:032x}", 900_000);
    connection
        .execute(
            "INSERT INTO windows_views(rowid,doc,position,view) VALUES(?1,?2,200,x'ff0011')",
            params![i64::MIN, orphan_id],
        )
        .unwrap();
    connection.execute("INSERT INTO recovery_heads(doc,checkpoint_rev,durable_rev,byte_len,checkpoint_hash) VALUES(?1,0,0,1,x'00')",[&orphan_id]).unwrap();
    connection
        .execute(
            "INSERT INTO windows_views(rowid,doc,position,view) VALUES(-3,NULL,201,NULL)",
            [],
        )
        .unwrap();
    connection.execute("INSERT INTO windows_views(rowid,doc,position,view) VALUES(0,x'ff',202,zeroblob(70000))",[]).unwrap();
    let huge_id = "界".repeat(70_000);
    connection
        .execute(
            "INSERT INTO windows_views(rowid,doc,position,view) VALUES(7,?1,203,123)",
            [&huge_id],
        )
        .unwrap();
    connection.execute("INSERT INTO windows_views(rowid,doc,position,view) VALUES(8,CAST(x'fe' AS TEXT),204,x'ffff')",[]).unwrap();
    drop(connection);

    let service = RecoveryHandle::start(&database).unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.layout.views.len(), 1);
    assert_eq!(
        manifest.recovery,
        RecoveryCatalogSummary {
            total_eligible: 9,
            admitted: 1,
            closed: 0,
            unplaced_checkpoints: 1,
            unplaced_other: 7
        }
    );
    let first = receive(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 2,
            })
            .unwrap(),
    );
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.page_unreadable, 1);
    assert_eq!(first.entries[0].key.origin, RecoveryEntryOrigin::Document);
    assert_eq!(first.entries[0].key.rowid, i64::MIN);
    assert_eq!(first.entries[1].key.rowid, -3);
    let initial_cursor = first.next.unwrap();
    assert_eq!(initial_cursor.document_upper_rowid, Some(8));
    assert_eq!(initial_cursor.legacy_orphan_upper_rowid, Some(104));
    let connection = Connection::open(&database).unwrap();
    let meta = serde_json::to_string(&DocMeta::default()).unwrap();
    connection
        .execute(
            "INSERT INTO documents(rowid,id,gen,state,meta) VALUES(200,?1,0,1,?2)",
            params![format!("{:032x}", 900_001), meta],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO legacy_v1_windows_views(rowid,doc,position,view) VALUES(201,?1,206,'{}')",
            [format!("{:032x}", 900_001)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO legacy_v1_windows_views(rowid,doc,position,view) VALUES(200,?1,205,'{}')",
            [format!("{:032x}", 900_002)],
        )
        .unwrap();

    let mut records = first.entries;
    let mut cursor = Some(initial_cursor);
    while let Some(next) = cursor {
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest {
                    cursor: Some(next),
                    limit: 2,
                })
                .unwrap(),
        );
        assert!(!page.entries.is_empty());
        assert!(page.entries.len() <= 2);
        assert_eq!(page.summary.total_eligible, 11);
        if let Some(next) = page.next {
            assert_eq!(
                next.document_upper_rowid,
                initial_cursor.document_upper_rowid
            );
            assert_eq!(
                next.legacy_orphan_upper_rowid,
                initial_cursor.legacy_orphan_upper_rowid
            );
        }
        records.extend(page.entries);
        cursor = page.next;
    }
    assert_eq!(records.len(), 9);
    assert_eq!(
        records
            .iter()
            .map(|entry| entry.key)
            .collect::<HashSet<_>>()
            .len(),
        9
    );
    assert_eq!(
        records
            .iter()
            .filter(|entry| entry.key.origin == RecoveryEntryOrigin::Document)
            .map(|entry| entry.key.rowid)
            .collect::<Vec<_>>(),
        vec![i64::MIN, -3, 0, 7]
    );
    assert_eq!(
        records
            .iter()
            .filter(|entry| entry.key.origin == RecoveryEntryOrigin::LegacyOrphan)
            .map(|entry| entry.key.rowid)
            .collect::<Vec<_>>(),
        vec![i64::MIN, -3, 0, 7, 8]
    );
    assert_eq!(
        records.iter().filter(|entry| entry.stub.is_err()).count(),
        8
    );
    for entry in &records {
        if entry.key.origin == RecoveryEntryOrigin::LegacyOrphan {
            assert!(entry.key.lifecycle_gen.is_none());
            assert!(entry.last_placement.is_none());
            assert_eq!(entry.stub.as_ref().unwrap_err().kind, ErrorKind::Corrupt);
        }
        if entry.key.rowid == i64::MIN || entry.key.rowid == 0 {
            let key = entry.key;
            let point = RecoveryCursor {
                origin: key.origin,
                after_rowid: key.rowid.checked_sub(1),
                document_upper_rowid: (key.origin == RecoveryEntryOrigin::Document)
                    .then_some(key.rowid),
                legacy_orphan_upper_rowid: (key.origin == RecoveryEntryOrigin::LegacyOrphan)
                    .then_some(key.rowid),
            };
            let result = receive(
                service
                    .request_recovery_page(RecoveryPageRequest {
                        cursor: Some(point),
                        limit: 1,
                    })
                    .unwrap(),
            );
            assert_eq!(result.entries.len(), 1);
            assert_eq!(result.entries[0].key, key);
            assert!(result.next.is_none());
        }
    }
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM legacy_v1_windows_views", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        12
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT doc FROM legacy_v1_windows_views WHERE rowid=7",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        huge_id
    );
    assert!(
        connection
            .query_row(
                "SELECT view=zeroblob(70000) FROM legacy_v1_windows_views WHERE rowid=0",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap()
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM recovery_heads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );

    connection
        .execute(
            "INSERT INTO documents(rowid,id,gen,state,meta) VALUES(?1,?2,0,1,?3)",
            params![i64::MAX, format!("{:032x}", 900_003), meta],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO legacy_v1_windows_views(rowid,doc,position,view) VALUES(202,?1,207,'{}')",
            [format!("{:032x}", 900_003)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO legacy_v1_windows_views(rowid,doc,position,view) VALUES(?1,?2,206,'{}')",
            params![i64::MAX, format!("{:032x}", 900_004)],
        )
        .unwrap();
    for origin in [
        RecoveryEntryOrigin::Document,
        RecoveryEntryOrigin::LegacyOrphan,
    ] {
        let cursor = RecoveryCursor {
            origin,
            after_rowid: Some(i64::MAX - 1),
            document_upper_rowid: (origin == RecoveryEntryOrigin::Document).then_some(i64::MAX),
            legacy_orphan_upper_rowid: (origin == RecoveryEntryOrigin::LegacyOrphan)
                .then_some(i64::MAX),
        };
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest {
                    cursor: Some(cursor),
                    limit: 1,
                })
                .unwrap(),
        );
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].key.origin, origin);
        assert_eq!(page.entries[0].key.rowid, i64::MAX);
        assert!(page.next.is_none());
    }
    let refreshed = receive(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 64,
            })
            .unwrap(),
    );
    assert_eq!(refreshed.entries.len(), 13);
    assert!(refreshed.next.is_none());
    assert_eq!(
        refreshed.summary,
        RecoveryCatalogSummary {
            total_eligible: 13,
            admitted: 1,
            closed: 2,
            unplaced_checkpoints: 1,
            unplaced_other: 9
        }
    );
    receive(admit(|| service.shutdown()));
    drop(service);
    let readonly = read_recovery_page(
        &database,
        RecoveryPageRequest {
            cursor: None,
            limit: 64,
        },
    )
    .unwrap();
    assert_eq!(readonly.summary, refreshed.summary);
    assert_eq!(
        readonly
            .entries
            .iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>(),
        refreshed
            .entries
            .iter()
            .map(|entry| entry.key)
            .collect::<Vec<_>>()
    );
}

#[test]
fn corrupt_recovery_hashes_and_payloads_remain_bounded_and_keep_close_preserves_them() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("bounded-drafts.sqlite3");
    let service = RecoveryHandle::start(&database).unwrap();
    let (layout, targets, revision) = metadata_documents(&service, 6);
    for target in &targets {
        let job = snapshot(target.document.doc_id, 1, "small draft");
        admit(|| service.try_commit(job.clone()));
    }
    receive(admit(|| service.flush()));
    for index in [2, 5] {
        let mut job = snapshot(targets[index].document.doc_id, 2, "small draft!");
        job.deltas = vec![TextDelta {
            byte_range: 11..11,
            replacement: "!".into(),
        }];
        admit(|| service.try_commit(job.clone()));
    }
    receive(admit(|| service.flush()));
    let connection = Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM edit_transactions", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2
    );
    let mutations = [
        (
            "UPDATE recovery_heads SET checkpoint_hash=zeroblob(70000) WHERE doc=?1",
            "SELECT length(checkpoint_hash) FROM recovery_heads WHERE doc=?1",
            70_000,
        ),
        (
            "UPDATE checkpoint_chunks SET checksum=zeroblob(70000) WHERE doc=?1",
            "SELECT length(checksum) FROM checkpoint_chunks WHERE doc=?1",
            70_000,
        ),
        (
            "UPDATE edit_transactions SET checksum=zeroblob(70000) WHERE doc=?1",
            "SELECT length(checksum) FROM edit_transactions WHERE doc=?1",
            70_000,
        ),
        (
            "UPDATE checkpoint_chunks SET content=CAST(zeroblob(70000) AS TEXT) WHERE doc=?1",
            "SELECT length(CAST(content AS BLOB)) FROM checkpoint_chunks WHERE doc=?1",
            70_000,
        ),
        (
            "UPDATE checkpoint_chunks SET content=zeroblob(2000000) WHERE doc=?1",
            "SELECT length(content) FROM checkpoint_chunks WHERE doc=?1",
            2_000_000,
        ),
        (
            "UPDATE edit_transactions SET deltas=zeroblob(4000000) WHERE doc=?1",
            "SELECT length(deltas) FROM edit_transactions WHERE doc=?1",
            4_000_000,
        ),
    ];
    for (target, (mutation, _, _)) in targets.iter().zip(mutations) {
        connection
            .execute(mutation, [format!("{:032x}", target.document.doc_id.0)])
            .unwrap();
    }
    let page = receive(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 64,
            })
            .unwrap(),
    );
    assert_eq!(page.entries.len(), 6);
    assert!(
        page.entries
            .iter()
            .all(|entry| entry.stub.as_ref().is_ok_and(|stub| stub.has_checkpoint))
    );
    for target in &targets {
        assert_eq!(
            response(
                service
                    .request_hydrate(target.document, CancelToken::new())
                    .unwrap()
            )
            .unwrap_err()
            .kind,
            ErrorKind::Corrupt
        );
    }
    let entries = targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            entry(
                *target,
                Some(Revision(if index == 2 || index == 5 { 2 } else { 1 })),
            )
        })
        .collect();
    let closed = receive(
        service
            .close_views(batch(&layout, revision, entries))
            .unwrap(),
    );
    assert_eq!(closed.closed.len(), 6);
    assert!(
        closed
            .closed
            .iter()
            .all(|record| record.document.has_checkpoint)
    );
    receive(admit(|| service.shutdown()));
    drop(service);
    for (target, (_, query, length)) in targets.iter().zip(mutations) {
        assert_eq!(
            connection
                .query_row(
                    query,
                    [format!("{:032x}", target.document.doc_id.0)],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            length
        );
        assert_eq!(
            read_recovery_draft(&database, target.document, &CancelToken::new())
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
    }
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM recovery_heads", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        6
    );
}

#[test]
fn legacy_non_text_and_invalid_utf8_payloads_are_preserved_and_isolated() {
    for active_value in ["x'ff00'", "CAST(x'ff00' AS TEXT)"] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("legacy-corrupt-columns.sqlite3");
        legacy_database(&database, 5);
        let connection = Connection::open(&database).unwrap();
        connection
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        // A corrupt non-strict v1 schema can contain NULL metadata. Only this
        // temporary fixture is rebuilt; the production migration must retain it.
        connection.execute_batch(
            "ALTER TABLE documents RENAME TO fixture_documents;
             CREATE TABLE documents(id TEXT PRIMARY KEY,gen INTEGER NOT NULL,state INTEGER NOT NULL,meta TEXT,saved_rev INTEGER NOT NULL DEFAULT -1,saved_meta TEXT);
             INSERT INTO documents SELECT * FROM fixture_documents;
             DROP TABLE fixture_documents;"
        ).unwrap();
        connection
            .execute(
                "UPDATE windows_views SET view=x'ff00' WHERE doc=?1",
                [format!("{:032x}", 1)],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE windows_views SET view=CAST(x'ff00' AS TEXT) WHERE doc=?1",
                [format!("{:032x}", 2)],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE documents SET meta=NULL WHERE id=?1",
                [format!("{:032x}", 4)],
            )
            .unwrap();
        let invalid_id = vec![255u8; 32];
        connection
            .execute(
                "UPDATE documents SET id=CAST(?1 AS TEXT) WHERE id=?2",
                params![invalid_id, format!("{:032x}", 5)],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE windows_views SET doc=CAST(?1 AS TEXT) WHERE doc=?2",
                params![invalid_id, format!("{:032x}", 5)],
            )
            .unwrap();
        connection.execute(
            "INSERT INTO recovery_heads(doc,checkpoint_rev,durable_rev,byte_len,checkpoint_hash) VALUES(CAST(?1 AS TEXT),0,0,0,zeroblob(32))",
            [&invalid_id],
        ).unwrap();
        connection
            .execute(
                &format!("UPDATE session SET value={active_value} WHERE key='active'"),
                [],
            )
            .unwrap();
        drop(connection);

        let service = RecoveryHandle::start(&database).unwrap();
        let manifest = receive(service.request_manifest().unwrap());
        assert_eq!(manifest.documents.len(), 3);
        assert_eq!(manifest.layout.views.len(), 3);
        for number in 1..=3 {
            let view = manifest
                .layout
                .views
                .values()
                .find(|view| view.document.doc_id == DocumentId(number))
                .unwrap();
            let expected = if number <= 2 {
                ViewState::default()
            } else {
                ViewState {
                    cursor_byte: 2,
                    scroll_byte: 0,
                }
            };
            assert_eq!(view.view, expected);
        }
        let active = manifest.layout.active_pane().active_view.unwrap();
        assert_eq!(
            manifest.layout.views[&active].document.doc_id,
            DocumentId(1)
        );
        let page = receive(
            service
                .request_recovery_page(RecoveryPageRequest {
                    cursor: None,
                    limit: 64,
                })
                .unwrap(),
        );
        assert_eq!(page.entries.len(), 5);
        assert_eq!(page.page_unreadable, 2);
        assert_eq!(
            page.summary,
            RecoveryCatalogSummary {
                total_eligible: 5,
                admitted: 3,
                closed: 0,
                unplaced_checkpoints: 1,
                unplaced_other: 1,
            }
        );
        let null_meta = page
            .entries
            .iter()
            .find(|entry| entry.key.doc_id == Some(DocumentId(4)))
            .unwrap();
        assert_eq!(
            null_meta.stub.as_ref().unwrap_err().kind,
            ErrorKind::Corrupt
        );
        assert!(
            null_meta
                .stub
                .as_ref()
                .unwrap_err()
                .message
                .contains("null")
        );
        let bad_id = page
            .entries
            .iter()
            .find(|entry| entry.key.doc_id.is_none())
            .unwrap();
        assert_eq!(bad_id.stub.as_ref().unwrap_err().kind, ErrorKind::Corrupt);

        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        let raw_active: (String, Vec<u8>) = connection.query_row(
            "SELECT typeof(value),CAST(value AS BLOB) FROM legacy_v1_session WHERE key='active'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(
            raw_active.0,
            if active_value.starts_with("CAST") {
                "text"
            } else {
                "blob"
            }
        );
        assert_eq!(raw_active.1, vec![255, 0]);
        for (number, expected_type) in [(1, "blob"), (2, "text")] {
            let raw: (String, Vec<u8>) = connection.query_row(
                "SELECT typeof(view),CAST(view AS BLOB) FROM legacy_v1_windows_views WHERE doc=?1",
                [format!("{number:032x}")], |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            assert_eq!(raw.0, expected_type);
            assert_eq!(raw.1, vec![255, 0]);
        }
        assert!(
            connection
                .query_row(
                    "SELECT meta IS NULL FROM documents WHERE id=?1",
                    [format!("{:032x}", 4)],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap()
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT CAST(doc AS BLOB) FROM legacy_v1_windows_views WHERE position=4",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .unwrap(),
            invalid_id
        );
        receive(admit(|| service.shutdown()));
    }
}

fn raw_layout_payloads(
    connection: &Connection,
    revision: LayoutRevision,
) -> (Vec<u8>, Vec<(String, Vec<u8>)>) {
    let revision = i64::try_from(revision.0).unwrap();
    let tree = connection
        .query_row(
            "SELECT CAST(tree AS BLOB) FROM layout_states WHERE id=?1",
            [revision],
            |row| row.get(0),
        )
        .unwrap();
    let mut statement = connection.prepare(
        "SELECT view_id,CAST(view_state AS BLOB) FROM layout_views WHERE view_set=(SELECT view_set FROM layout_states WHERE id=?1) ORDER BY view_id",
    ).unwrap();
    let views = statement
        .query_map([revision], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (tree, views)
}

#[test]
fn future_layout_fields_and_versions_refuse_before_wal_without_rewriting_snapshots() {
    for (slot, case) in [
        ("current", "tree"),
        ("current", "node"),
        ("current", "pane"),
        ("current", "group"),
        ("current", "view"),
        ("current", "column"),
        ("current", "version"),
        ("current", "root"),
        ("current", "schema"),
        ("last_valid", "tree"),
        ("last_valid", "view"),
        ("current", "recognized"),
    ] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("future-layout.sqlite3");
        let service = RecoveryHandle::start(&database).unwrap();
        let (layout, _, _) = metadata_documents(&service, 2);
        persist_test_layout(&service, &layout, Vec::new());
        receive(admit(|| service.shutdown()));
        drop(service);
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=DELETE;")
            .unwrap();
        let revision = LayoutRevision(
            connection
                .query_row(
                    "SELECT layout_id FROM layout_heads WHERE slot=?1",
                    [slot],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
                .try_into()
                .unwrap(),
        );
        let original = raw_layout_payloads(&connection, revision);
        let mut tree: serde_json::Value = serde_json::from_slice(&original.0).unwrap();
        let pointer = match case {
            "tree" => Some(""),
            "node" => Some("/root/Pane"),
            "pane" => Some("/root/Pane/pane"),
            "group" => Some("/groups/0"),
            _ => None,
        };
        if let Some(pointer) = pointer {
            tree.pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(
                    "future_layout".into(),
                    serde_json::json!({"preserve": [1, 2]}),
                );
        }
        match case {
            "version" => tree["format_version"] = serde_json::json!(99),
            "root" => tree["root"] = serde_json::json!({"Split": {"children": []}}),
            "column" => {
                connection
                    .execute(
                        "UPDATE layout_states SET format_version=99 WHERE id=?1",
                        [i64::try_from(revision.0).unwrap()],
                    )
                    .unwrap();
            }
            "schema" => connection.pragma_update(None, "user_version", 3).unwrap(),
            "view" => {
                let mut state: serde_json::Value =
                    serde_json::from_slice(&original.1[0].1).unwrap();
                state["future_selection"] = serde_json::json!([3, 17]);
                connection.execute(
                    "UPDATE layout_views SET view_state=?3 WHERE view_set=(SELECT view_set FROM layout_states WHERE id=?1) AND view_id=?2",
                    params![i64::try_from(revision.0).unwrap(), original.1[0].0, state.to_string()],
                ).unwrap();
            }
            _ => {}
        }
        if pointer.is_some() || matches!(case, "version" | "root") {
            connection
                .execute(
                    "UPDATE layout_states SET tree=?2 WHERE id=?1",
                    params![
                        i64::try_from(revision.0).unwrap(),
                        serde_json::to_vec(&tree).unwrap()
                    ],
                )
                .unwrap();
        }
        let payloads = raw_layout_payloads(&connection, revision);
        let before = disk_images(&database);
        let layout_count: i64 = connection
            .query_row("SELECT count(*) FROM layout_states", [], |row| row.get(0))
            .unwrap();
        if case == "recognized" {
            let service = RecoveryHandle::start(&database).unwrap();
            let manifest = receive(service.request_manifest().unwrap());
            assert_eq!(manifest.layout, layout);
            receive(
                service
                    .write_session(SessionWrite {
                        sequence: LayoutSeq(20),
                        expected_revision: manifest.layout_revision,
                        layout: LayoutPayload::Structural(manifest.layout),
                        document_changes: Vec::new(),
                        preserve_source_layout: None,
                    })
                    .unwrap(),
            );
            assert_eq!(raw_layout_payloads(&connection, revision), payloads);
            let previous: i64 = connection
                .query_row(
                    "SELECT layout_id FROM layout_heads WHERE slot='last_valid'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(previous, i64::try_from(revision.0).unwrap());
            receive(admit(|| service.shutdown()));
        } else {
            assert_eq!(
                start_error(&database, FaultInjector::default()).kind,
                ErrorKind::Unsupported,
                "{slot}/{case}",
            );
            assert_eq!(disk_images(&database), before, "{slot}/{case}");
            assert_eq!(raw_layout_payloads(&connection, revision), payloads);
            assert_eq!(
                connection
                    .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
                    .unwrap(),
                "delete"
            );
            assert_eq!(
                connection
                    .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                    .unwrap(),
                if case == "schema" { 3 } else { 2 }
            );
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM layout_states", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                layout_count
            );
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM preserved_layouts", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn supported_corrupt_layouts_keep_raw_snapshots_through_fallback_and_direct_installation() {
    for corruption in [
        "oversized_tree",
        "non_json_tree",
        "truncated_tree",
        "missing_tree_key",
        "wrong_tree_type",
        "missing_view_key",
        "wrong_view_type",
    ] {
        for route in [
            "restart_previous",
            "restart_metadata",
            "restart_both_corrupt",
            "direct_install",
        ] {
            let directory = TempDir::new().unwrap();
            let database = directory.path().join("corrupt-layout.sqlite3");
            let service = RecoveryHandle::start(&database).unwrap();
            let (previous_layout, targets, previous_revision) = metadata_documents(&service, 2);
            let mut current_layout = previous_layout.clone();
            current_layout
                .apply_group_command(GroupCommand::Create {
                    name: "Current only".into(),
                    color: GroupColor::Blue,
                    before: None,
                    initial_view: Some(targets[0]),
                })
                .unwrap();
            let current_revision = persist_test_layout(&service, &current_layout, Vec::new());
            let service = if route == "direct_install" {
                Some(service)
            } else {
                receive(admit(|| service.shutdown()));
                drop(service);
                None
            };
            let connection = Connection::open(&database).unwrap();
            if route != "direct_install" {
                connection
                    .execute_batch("PRAGMA journal_mode=DELETE;")
                    .unwrap();
            }
            let mut tree = serde_json::to_vec(&current_layout.tree).unwrap();
            match corruption {
                "oversized_tree" => tree.resize(MAX_LAYOUT_TREE_BYTES + 1, b' '),
                "non_json_tree" => tree = b"not-json".to_vec(),
                "truncated_tree" => tree = b"{".to_vec(),
                "missing_tree_key" | "wrong_tree_type" => {
                    let mut value: serde_json::Value = serde_json::from_slice(&tree).unwrap();
                    if corruption == "missing_tree_key" {
                        value.as_object_mut().unwrap().remove("root");
                    } else {
                        value["groups"][1]["name"] = serde_json::json!(true);
                    }
                    tree = serde_json::to_vec(&value).unwrap();
                }
                _ => {}
            }
            connection
                .execute(
                    "UPDATE layout_states SET tree=?2 WHERE id=?1",
                    params![i64::try_from(current_revision.0).unwrap(), tree],
                )
                .unwrap();
            if matches!(corruption, "missing_view_key" | "wrong_view_type") {
                let state = if corruption == "missing_view_key" {
                    r#"{"cursor_byte":0}"#
                } else {
                    r#"{"cursor_byte":"not a byte offset","scroll_byte":0}"#
                };
                connection.execute(
                    "UPDATE layout_views SET view_state=?3 WHERE view_set=(SELECT view_set FROM layout_states WHERE id=?1) AND view_id=?2",
                    params![i64::try_from(current_revision.0).unwrap(), targets[0].view_id.key(), state],
                ).unwrap();
            }
            if route == "restart_metadata" {
                connection
                    .execute("DELETE FROM layout_heads WHERE slot='last_valid'", [])
                    .unwrap();
            }
            let previous_payloads = if route == "restart_both_corrupt" {
                connection
                    .execute(
                        "UPDATE layout_states SET tree=x'7b' WHERE id=?1",
                        [i64::try_from(previous_revision.0).unwrap()],
                    )
                    .unwrap();
                Some(raw_layout_payloads(&connection, previous_revision))
            } else {
                None
            };
            let payloads = raw_layout_payloads(&connection, current_revision);
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM preserved_layouts", [], |row| row
                        .get::<_, i64>(0),)
                    .unwrap(),
                0
            );
            let service = service.unwrap_or_else(|| RecoveryHandle::start(&database).unwrap());
            let restored = if route == "direct_install" {
                // The worker already admitted an earlier healthy manifest. Do not
                // request a new one: installation itself must pin the damaged head.
                current_layout.clone()
            } else {
                let manifest = receive(service.request_manifest().unwrap());
                assert_eq!(manifest.layout_revision, current_revision);
                assert!(!manifest.warnings.is_empty());
                assert_eq!(
                    manifest
                        .documents
                        .iter()
                        .map(|stub| stub.doc_id)
                        .collect::<HashSet<_>>(),
                    targets
                        .iter()
                        .map(|target| target.document.doc_id)
                        .collect::<HashSet<_>>()
                );
                if route == "restart_previous" {
                    assert_eq!(manifest.layout, previous_layout);
                } else {
                    assert_eq!(manifest.layout.tree.groups.len(), 1);
                }
                manifest.layout
            };
            let mut revision = current_revision;
            for sequence in 30..32 {
                revision = receive(
                    service
                        .write_session(SessionWrite {
                            sequence: LayoutSeq(sequence),
                            expected_revision: revision,
                            layout: LayoutPayload::Structural(restored.clone()),
                            document_changes: Vec::new(),
                            preserve_source_layout: None,
                        })
                        .unwrap(),
                )
                .revision;
                assert_eq!(
                    raw_layout_payloads(&connection, current_revision),
                    payloads,
                    "{corruption}/{route}"
                );
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT count(*) FROM preserved_layouts WHERE layout_id=?1",
                            [i64::try_from(current_revision.0).unwrap()],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap(),
                    1
                );
                if let Some(previous_payloads) = &previous_payloads {
                    assert_eq!(
                        &raw_layout_payloads(&connection, previous_revision),
                        previous_payloads
                    );
                    assert_eq!(
                        connection
                            .query_row(
                                "SELECT count(*) FROM preserved_layouts WHERE layout_id=?1",
                                [i64::try_from(previous_revision.0).unwrap()],
                                |row| row.get::<_, i64>(0),
                            )
                            .unwrap(),
                        1
                    );
                }
            }
            assert_eq!(
                receive(service.request_manifest().unwrap()).layout,
                restored
            );
            receive(admit(|| service.shutdown()));
        }
    }
}

#[test]
fn newly_unsupported_current_layout_aborts_install_and_metadata_atomically() {
    for field in ["tree", "view"] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("late-future.sqlite3");
        let service = RecoveryHandle::start(&database).unwrap();
        let (layout, targets, revision) = metadata_documents(&service, 2);
        let connection = Connection::open(&database).unwrap();
        if field == "tree" {
            let mut tree = serde_json::to_value(&layout.tree).unwrap();
            tree["future_layout"] = serde_json::json!({"split": [1, 2]});
            connection
                .execute(
                    "UPDATE layout_states SET tree=?2 WHERE id=?1",
                    params![
                        i64::try_from(revision.0).unwrap(),
                        serde_json::to_vec(&tree).unwrap()
                    ],
                )
                .unwrap();
        } else {
            connection.execute(
                "UPDATE layout_views SET view_state='{\"cursor_byte\":0,\"scroll_byte\":0,\"future_selection\":[1,2]}' WHERE view_set=(SELECT view_set FROM layout_states WHERE id=?1)",
                [i64::try_from(revision.0).unwrap()],
            ).unwrap();
        }
        let payloads = raw_layout_payloads(&connection, revision);
        let original_meta: Vec<u8> = connection
            .query_row(
                "SELECT CAST(meta AS BLOB) FROM documents WHERE id=?1",
                [format!("{:032x}", targets[0].document.doc_id.0)],
                |row| row.get(0),
            )
            .unwrap();
        let before_count: i64 = connection
            .query_row("SELECT count(*) FROM layout_states", [], |row| row.get(0))
            .unwrap();
        let result = response(
            service
                .write_session(SessionWrite {
                    sequence: LayoutSeq(40),
                    expected_revision: revision,
                    layout: LayoutPayload::Structural(layout),
                    document_changes: vec![DocumentChange::Update(DocumentMetaRecord {
                        key: targets[0].document,
                        meta: DocMeta {
                            title: "Must roll back".into(),
                            ..DocMeta::default()
                        },
                    })],
                    preserve_source_layout: None,
                })
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(result.kind, ErrorKind::Unsupported);
        assert_eq!(raw_layout_payloads(&connection, revision), payloads);
        assert_eq!(
            connection
                .query_row(
                    "SELECT CAST(meta AS BLOB) FROM documents WHERE id=?1",
                    [format!("{:032x}", targets[0].document.doc_id.0)],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .unwrap(),
            original_meta
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM layout_states", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            before_count
        );
        assert_eq!(
            response(service.request_manifest().unwrap())
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM preserved_layouts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        receive(admit(|| service.shutdown()));
    }
}

#[test]
fn installing_over_a_healthy_current_preserves_or_refuses_a_damaged_previous_head() {
    for unsupported in [false, true] {
        let directory = TempDir::new().unwrap();
        let database = directory.path().join("damaged-previous.sqlite3");
        let service = RecoveryHandle::start(&database).unwrap();
        let (layout, _, previous_revision) = metadata_documents(&service, 2);
        let current_revision = persist_test_layout(&service, &layout, Vec::new());
        let connection = Connection::open(&database).unwrap();
        let body = if unsupported {
            let mut value = serde_json::to_value(&layout.tree).unwrap();
            value["future_previous"] = serde_json::json!(true);
            serde_json::to_vec(&value).unwrap()
        } else {
            b"not-json".to_vec()
        };
        connection
            .execute(
                "UPDATE layout_states SET tree=?2 WHERE id=?1",
                params![i64::try_from(previous_revision.0).unwrap(), body],
            )
            .unwrap();
        let previous_payloads = raw_layout_payloads(&connection, previous_revision);
        let current_payloads = raw_layout_payloads(&connection, current_revision);
        // No new manifest observes the damaged previous head before installation.
        let mut revision = current_revision;
        for sequence in 50..52 {
            let result = response(
                service
                    .write_session(SessionWrite {
                        sequence: LayoutSeq(sequence),
                        expected_revision: revision,
                        layout: LayoutPayload::Structural(layout.clone()),
                        document_changes: Vec::new(),
                        preserve_source_layout: None,
                    })
                    .unwrap(),
            );
            if unsupported {
                assert_eq!(result.unwrap_err().kind, ErrorKind::Unsupported);
                assert_eq!(
                    raw_layout_payloads(&connection, current_revision),
                    current_payloads
                );
                assert_eq!(
                    connection
                        .query_row(
                            "SELECT layout_id FROM layout_heads WHERE slot='current'",
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap(),
                    i64::try_from(current_revision.0).unwrap()
                );
            } else {
                revision = result.unwrap().revision;
            }
            assert_eq!(
                raw_layout_payloads(&connection, previous_revision),
                previous_payloads
            );
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM preserved_layouts WHERE layout_id=?1",
                        [i64::try_from(previous_revision.0).unwrap()],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                i64::from(!unsupported)
            );
        }
        receive(admit(|| service.shutdown()));
    }
}
