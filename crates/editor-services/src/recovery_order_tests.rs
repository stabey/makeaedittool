use super::*;
use tempfile::TempDir;

// Exercise real admission stamps and SQLite transactions in deliberately chosen
// worker orders; no sleeps or intermediate flushes hide the metadata races.
struct Fixture {
    dir: TempDir,
    conn: Connection,
    handle: RecoveryHandle,
    commands: Receiver<Command>,
    events: Sender<RecoveryEvent>,
    cache: SnapshotCache,
    layout: SessionLayout,
    next_base: LayoutRevision,
}
impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let mut conn =
            layout_store::open_database(&dir.path().join("db"), &FaultInjector::default()).unwrap();
        let manifest = layout_store::manifest(&mut conn).unwrap();
        let (commands, receiver) = bounded(32);
        let (events, event_receiver) = bounded(256);
        let handle = RecoveryHandle {
            inner: Arc::new(HandleInner {
                commands,
                events: event_receiver,
                mailbox: Arc::new(Mutex::new(Mailbox {
                    pending: HashMap::new(),
                    retained_bytes: 0,
                    inflight_bytes: 0,
                    accepting: true,
                    manifest_available: true,
                    next_meta_seq: 0,
                    meta_state: HashMap::new(),
                })),
                last_error: Arc::new(Mutex::new(None)),
                faults: FaultInjector::default(),
            }),
        };
        Self {
            dir,
            conn,
            handle,
            commands: receiver,
            events,
            cache: SnapshotCache::default(),
            layout: manifest.layout,
            next_base: manifest.layout_revision,
        }
    }
    fn queue_metadata(&mut self, id: DocumentId, meta: DocMeta) -> Result<(), ServiceError> {
        let key = key(id);
        let is_new = !self.layout.views.values().any(|v| v.document == key);
        if is_new {
            self.layout.insert_view(
                LayoutView {
                    id: ViewId::new(),
                    document: key,
                    group_id: GroupId::UNGROUPED,
                    view: ViewState {
                        cursor_byte: 3,
                        scroll_byte: 1,
                    },
                },
                None,
                true,
            )?;
        }
        let record = DocumentMetaRecord { key, meta };
        let write = SessionWrite {
            sequence: LayoutSeq(self.next_base.0),
            expected_revision: self.next_base,
            layout: LayoutPayload::Structural(self.layout.clone()),
            document_changes: vec![if is_new {
                DocumentChange::Create(record)
            } else {
                DocumentChange::Update(record)
            }],
            preserve_source_layout: None,
        };
        self.handle.write_session(write)?;
        self.next_base.0 += 1;
        Ok(())
    }
    fn drain(&mut self) -> Result<(), ServiceError> {
        drain(
            &mut self.conn,
            &self.handle.inner.mailbox,
            &self.events,
            &self.handle.inner.last_error,
            &self.handle.inner.faults,
            &mut self.cache,
        )
    }
    fn apply_next_layout(&mut self) -> Result<LayoutAck, ServiceError> {
        loop {
            match self.commands.try_recv().expect("admitted layout command") {
                Command::Wake => {}
                Command::Layout {
                    write,
                    meta_seq,
                    reply,
                } => {
                    let result = layout_store::write_session(
                        &mut self.conn,
                        &write,
                        meta_seq,
                        &self.handle.inner.mailbox,
                        &self.handle.inner.faults,
                    );
                    let mut mailbox = self.handle.inner.mailbox.lock().unwrap();
                    for change in &write.document_changes {
                        let key = change.key();
                        if result.is_ok() {
                            mailbox.record_meta_applied(key.doc_id, key.lifecycle_gen, meta_seq);
                        }
                        mailbox.release_meta(key.doc_id);
                    }
                    let _ = reply.send(result.clone());
                    return result;
                }
                _ => panic!("unexpected command"),
            }
        }
    }
    fn draft(&self, id: DocumentId) -> HydratedDraft {
        hydrate(&self.conn, key(id), &CancelToken::new()).unwrap()
    }
    fn assert_idle(&self) {
        let mailbox = self.handle.inner.mailbox.lock().unwrap();
        assert!(mailbox.meta_state.is_empty());
        assert!(mailbox.pending.is_empty());
        assert_eq!(mailbox.retained_bytes, 0);
        assert_eq!(mailbox.inflight_bytes, 0);
    }
}
fn key(id: DocumentId) -> DocumentKey {
    DocumentKey {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
    }
}
fn current(label: &str, encoding: EncodingSpec, line_ending: LineEnding) -> DocMeta {
    DocMeta {
        title: label.into(),
        encoding,
        line_ending,
        language_override: Some(label.into()),
        ..DocMeta::default()
    }
}
fn commit(id: DocumentId, revision: u64, meta: DocMeta) -> RecoveryCommit {
    RecoveryCommit {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
        base_rev: Revision(revision.saturating_sub(1)),
        new_rev: Revision(revision),
        rope: Rope::from_str("committed text\n"),
        deltas: Vec::new(),
        meta,
    }
}
fn assert_current(actual: &DocMeta, expected: &DocMeta) {
    assert_eq!(actual.title, expected.title);
    assert_eq!(actual.encoding, expected.encoding);
    assert_eq!(actual.line_ending, expected.line_ending);
    assert_eq!(actual.language_override, expected.language_override);
}

#[test]
fn newest_admitted_metadata_wins_in_both_worker_orders_with_saved_baseline() {
    for commit_first in [false, true] {
        for layout_first in [false, true] {
            let mut f = Fixture::new();
            let id = DocumentId::new();
            let identity = ContentIdentity::of_rope(&Rope::from_str("previous save"));
            let intent = SaveIntent {
                doc_id: id,
                lifecycle_gen: LifecycleGen(1),
                captured_rev: Revision(0),
                target_path: f.dir.path().join("saved.txt"),
                captured_identity: identity,
                captured_encoding: Some(EncodingSpec::Utf8Bom),
                captured_line_ending: Some(LineEnding::Lf),
                stage: SaveStage::Published,
                output_fingerprint: None,
            };
            record_intent(&mut f.conn, &intent, &f.handle.inner.faults).unwrap();
            let cm = current("rust", EncodingSpec::Utf8Bom, LineEnding::Crlf);
            let lm = current("python", EncodingSpec::Utf8, LineEnding::Lf);
            if commit_first {
                f.handle.try_commit(commit(id, 1, cm.clone())).unwrap();
                f.queue_metadata(id, lm.clone()).unwrap();
            } else {
                f.queue_metadata(id, lm.clone()).unwrap();
                f.handle.try_commit(commit(id, 1, cm.clone())).unwrap();
            }
            if layout_first {
                f.apply_next_layout().unwrap();
                f.drain().unwrap();
            } else {
                f.drain().unwrap();
                f.apply_next_layout().unwrap();
            }
            let expected = if commit_first { &lm } else { &cm };
            let draft = f.draft(id);
            assert_current(&draft.meta, expected);
            assert_eq!(draft.meta.saved_identity, Some(identity));
            assert_eq!(draft.meta.saved_encoding, Some(EncodingSpec::Utf8Bom));
            assert_eq!(draft.meta.saved_line_ending, Some(LineEnding::Lf));
            assert_eq!(draft.rope, "committed text\n");
            assert_eq!(draft.durable_rev, Revision(1));
            let manifest = layout_store::manifest(&mut f.conn).unwrap();
            assert_current(&manifest.documents[0].meta, expected);
            assert_eq!(
                manifest.layout.views[&manifest.layout.pane().tabs[0]]
                    .view
                    .cursor_byte,
                3
            );
            f.assert_idle();
        }
    }
}
#[test]
fn applied_stamp_never_regresses_across_three_out_of_order_sources() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    f.queue_metadata(id, current("old", EncodingSpec::Utf8, LineEnding::Lf))
        .unwrap();
    f.queue_metadata(id, current("middle", EncodingSpec::Utf8, LineEnding::Crlf))
        .unwrap();
    let newest = current("newest", EncodingSpec::Utf8Bom, LineEnding::Crlf);
    f.handle.try_commit(commit(id, 1, newest.clone())).unwrap();
    f.drain().unwrap();
    let applied = f.handle.inner.mailbox.lock().unwrap().meta_state[&id].applied;
    f.apply_next_layout().unwrap();
    assert_eq!(
        f.handle.inner.mailbox.lock().unwrap().meta_state[&id].applied,
        applied
    );
    f.apply_next_layout().unwrap();
    assert_current(&f.draft(id).meta, &newest);
    f.assert_idle();
}
#[test]
fn retry_keeps_original_stamp_and_newer_layout_survives() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    f.handle
        .try_commit(commit(id, 1, DocMeta::default()))
        .unwrap();
    let seq = f.handle.inner.mailbox.lock().unwrap().pending[&id].meta_seq;
    f.handle.inner.faults.arm(FaultPoint::RecoveryBeforeCommit);
    assert_eq!(f.drain().unwrap_err().kind, ErrorKind::Injected);
    {
        let m = f.handle.inner.mailbox.lock().unwrap();
        assert_eq!(m.pending[&id].meta_seq, seq);
        assert_eq!(m.meta_state[&id].outstanding, 1);
        assert!(m.meta_state[&id].applied.is_none());
    }
    let newest = current("python", EncodingSpec::Utf8Bom, LineEnding::Crlf);
    f.queue_metadata(id, newest.clone()).unwrap();
    f.apply_next_layout().unwrap();
    f.drain().unwrap();
    assert_current(&f.draft(id).meta, &newest);
    f.assert_idle();
    let after = current("rust", EncodingSpec::Utf8, LineEnding::Lf);
    f.handle.try_commit(commit(id, 2, after.clone())).unwrap();
    f.drain().unwrap();
    assert_current(&f.draft(id).meta, &after);
    f.assert_idle();
}
#[test]
fn superseded_inflight_failure_releases_only_its_own_source() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    f.handle
        .try_commit(commit(id, 1, DocMeta::default()))
        .unwrap();
    let (pending, preserve) = take_pending(&f.handle.inner.mailbox, id).unwrap();
    let newest = current("python", EncodingSpec::Utf8Bom, LineEnding::Crlf);
    f.handle.try_commit(commit(id, 2, newest.clone())).unwrap();
    assert_eq!(
        f.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
        2
    );
    f.handle.inner.faults.arm(FaultPoint::RecoveryBeforeCommit);
    let result = commit_one(
        &mut f.conn,
        &pending.commit,
        preserve,
        &f.handle.inner.faults,
        &mut f.cache,
    );
    assert_eq!(result.as_ref().unwrap_err().kind, ErrorKind::Injected);
    finish_commit(&f.handle.inner.mailbox, pending, &result);
    assert_eq!(
        f.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
        1
    );
    f.drain().unwrap();
    assert_current(&f.draft(id).meta, &newest);
    assert_eq!(f.draft(id).durable_rev, Revision(2));
    f.assert_idle();
}
#[test]
fn failed_layout_transaction_does_not_advance_metadata_stamp() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    let old = current("old", EncodingSpec::Utf8, LineEnding::Lf);
    f.handle.try_commit(commit(id, 1, old.clone())).unwrap();
    f.queue_metadata(
        id,
        current("failed", EncodingSpec::Utf8Bom, LineEnding::Crlf),
    )
    .unwrap();
    f.conn.execute_batch("CREATE TEMP TRIGGER reject_layout BEFORE INSERT ON layout_states BEGIN SELECT RAISE(ABORT,'layout failure'); END;").unwrap();
    assert_eq!(f.apply_next_layout().unwrap_err().kind, ErrorKind::Storage);
    {
        let m = f.handle.inner.mailbox.lock().unwrap();
        assert_eq!(m.meta_state[&id].outstanding, 1);
        assert!(m.meta_state[&id].applied.is_none());
    }
    f.conn.execute_batch("DROP TRIGGER reject_layout").unwrap();
    f.drain().unwrap();
    assert_current(&f.draft(id).meta, &old);
    f.assert_idle();
}
#[test]
fn metadata_refs_balance_on_coalescing_admission_failure_and_terminal_errors() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    for _ in 0..32 {
        f.handle.inner.commands.try_send(Command::Wake).unwrap();
    }
    assert_eq!(
        f.queue_metadata(id, DocMeta::default()).unwrap_err().kind,
        ErrorKind::Busy
    );
    f.assert_idle();
    assert_eq!(f.commands.try_iter().count(), 32);
    f.handle
        .try_commit(commit(id, 1, DocMeta::default()))
        .unwrap();
    f.handle
        .try_commit(commit(id, 2, DocMeta::default()))
        .unwrap();
    assert_eq!(
        f.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
        1
    );
    f.drain().unwrap();
    f.assert_idle();
    f.handle
        .try_commit(commit(id, u64::MAX, DocMeta::default()))
        .unwrap();
    assert_eq!(f.drain().unwrap_err().kind, ErrorKind::InvalidInput);
    f.assert_idle();
}
#[test]
fn close_does_not_release_queued_sources_or_stamp_rejected_layouts() {
    let mut f = Fixture::new();
    let id = DocumentId::new();
    f.queue_metadata(id, DocMeta::default()).unwrap();
    let ack = f.apply_next_layout().unwrap();
    let target = f.layout.target(f.layout.pane().tabs[0]).unwrap();
    f.handle
        .try_commit(commit(id, 1, DocMeta::default()))
        .unwrap();
    f.queue_metadata(id, current("late", EncodingSpec::Utf8, LineEnding::Lf))
        .unwrap();
    let mut after = f.layout.clone();
    after.remove_views(&[target]).unwrap();
    layout_store::close_views(
        &mut f.conn,
        &CloseBatch {
            sequence: LayoutSeq(50),
            expected_revision: ack.revision,
            entries: vec![CloseEntry {
                target,
                disposition: CloseDisposition::Discard {
                    tombstone_gen: LifecycleGen(2),
                },
                required_durable_rev: None,
                expected_format: (EncodingSpec::Utf8, LineEnding::Lf),
                view: ViewState::default(),
                expected_identity: None,
            }],
            after,
            empty_pane_document: None,
        },
        &f.handle.inner.faults,
    )
    .unwrap();
    assert_eq!(
        f.handle.inner.mailbox.lock().unwrap().meta_state[&id].outstanding,
        2
    );
    assert_eq!(f.apply_next_layout().unwrap_err().kind, ErrorKind::Stale);
    {
        let m = f.handle.inner.mailbox.lock().unwrap();
        assert_eq!(m.meta_state[&id].outstanding, 1);
        assert!(m.meta_state[&id].applied.is_none());
    }
    f.drain().unwrap();
    f.assert_idle();
}
#[test]
fn missing_row_save_acknowledgement_uses_captured_current_format() {
    let mut f = Fixture::new();
    for captured in [false, true] {
        let intent = SaveIntent {
            doc_id: DocumentId::new(),
            lifecycle_gen: LifecycleGen(1),
            captured_rev: Revision(1),
            target_path: f.dir.path().join("saved.txt"),
            captured_identity: ContentIdentity::of_rope(&Rope::from_str("saved")),
            captured_encoding: captured.then_some(EncodingSpec::Utf8Bom),
            captured_line_ending: captured.then_some(LineEnding::Crlf),
            stage: SaveStage::Published,
            output_fingerprint: None,
        };
        record_intent(&mut f.conn, &intent, &f.handle.inner.faults).unwrap();
        let meta = layout_store::document_stub(&f.conn, &intent.doc_id.key())
            .unwrap()
            .meta;
        assert_eq!(meta.encoding, intent.captured_encoding.unwrap_or_default());
        assert_eq!(
            meta.line_ending,
            intent.captured_line_ending.unwrap_or_default()
        );
        assert_eq!(meta.saved_encoding, intent.captured_encoding);
        assert_eq!(meta.saved_line_ending, intent.captured_line_ending);
    }
}
