use editor_services::*;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use tempfile::TempDir;

fn receive<T>(reply: Reply<T>) -> T {
    reply
        .recv_timeout(Duration::from_secs(30))
        .expect("worker reply")
        .expect("successful worker operation")
}
fn admit<T>(mut operation: impl FnMut() -> Result<T, ServiceError>) -> T {
    let started = Instant::now();
    loop {
        match operation() {
            Ok(value) => return value,
            Err(error)
                if error.kind == ErrorKind::Busy && started.elapsed() < Duration::from_secs(30) =>
            {
                // A bounded service may reject either mailbox contention or queue pressure.
                // The caller retains the exact submission until it is accepted once.
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("recovery admission failed: {error}"),
        }
    }
}
fn snapshot(id: DocumentId, revision: u64, text: &str) -> RecoveryCommit {
    RecoveryCommit {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
        base_rev: Revision(revision.saturating_sub(1)),
        new_rev: Revision(revision),
        rope: Rope::from_str(text),
        deltas: vec![],
        meta: DocMeta {
            title: "Test draft".into(),
            ..Default::default()
        },
    }
}

fn document_key(id: DocumentId) -> DocumentKey {
    DocumentKey {
        doc_id: id,
        lifecycle_gen: LifecycleGen(1),
    }
}
fn add_layout_view(
    layout: &mut SessionLayout,
    key: DocumentKey,
    view: ViewState,
    active: bool,
) -> ViewTarget {
    let id = ViewId::new();
    layout
        .insert_view(
            LayoutView {
                id,
                document: key,
                group_id: GroupId::UNGROUPED,
                view,
            },
            None,
            active,
        )
        .unwrap();
    ViewTarget {
        view_id: id,
        document: key,
    }
}
fn persist_test_layout(
    service: &RecoveryHandle,
    layout: &SessionLayout,
    changes: Vec<DocumentChange>,
) -> LayoutRevision {
    let base = receive(service.request_manifest().unwrap()).layout_revision;
    receive(admit(|| {
        service.write_session(SessionWrite {
            sequence: LayoutSeq(1),
            expected_revision: base,
            layout: LayoutPayload::Structural(layout.clone()),
            document_changes: changes.clone(),
            preserve_source_layout: None,
        })
    }))
    .revision
}
fn close_test_view(
    service: &RecoveryHandle,
    target: ViewTarget,
    disposition: CloseDisposition,
    required: Option<Revision>,
) -> CloseAck {
    let manifest = receive(service.request_manifest().unwrap());
    let stub = manifest
        .documents
        .iter()
        .find(|stub| stub.key() == target.document)
        .unwrap();
    let view = manifest.layout.views[&target.view_id].view.clone();
    let mut after = manifest.layout.clone();
    after.remove_views(&[target]).unwrap();
    receive(admit(|| {
        service.close_views(CloseBatch {
            sequence: LayoutSeq(2),
            expected_revision: manifest.layout_revision,
            entries: vec![CloseEntry {
                target,
                disposition,
                required_durable_rev: required,
                expected_format: (stub.meta.encoding, stub.meta.line_ending),
                view: view.clone(),
                expected_identity: None,
            }],
            after: after.clone(),
            empty_pane_document: None,
        })
    }))
}
fn matches_for(
    text: &str,
    pattern: &str,
    block: usize,
    algorithm: SearchAlgorithm,
    cap: usize,
) -> (Vec<std::ops::Range<u64>>, bool, bool) {
    let service = SearchService::with_limit(1);
    let mut request = SearchRequest::new(
        QueryId(77),
        pattern.as_bytes().to_vec(),
        SearchTarget::EditableSnapshot {
            doc_id: DocumentId::new(),
            rev: Revision(1),
            rope: Rope::from_str(text),
        },
    );
    request.block_bytes = block;
    request.algorithm = algorithm;
    request.max_matches = cap;
    let task = service.start(request).expect("start search");
    let mut ranges = Vec::new();
    loop {
        match task
            .events
            .recv_timeout(Duration::from_secs(10))
            .expect("search event")
        {
            SearchEvent::Batch { query_id, matches } => {
                assert_eq!(query_id, QueryId(77));
                for hit in matches {
                    assert!(hit.preview.len() <= 256);
                    assert!(hit.preview_match_range.end <= hit.preview.len());
                    ranges.push(hit.byte_range);
                }
            }
            SearchEvent::Done {
                query_id,
                total_emitted,
                truncated,
                count_exact,
                ..
            } => {
                assert_eq!(query_id, QueryId(77));
                assert_eq!(total_emitted, ranges.len());
                return (ranges, truncated, count_exact);
            }
            event => panic!("unexpected search result: {event:?}"),
        }
    }
}
#[test]
fn literal_search_is_differentially_correct_at_every_small_boundary() {
    for text in [
        "aaaaa",
        "aaaaaa",
        "中文🙂\r\n中文🙂\r\n尾",
        "xababababa abababa ab",
        "prefix\r\nlongish line\r\nsuffix",
    ] {
        for pattern in ["aaa", "aba", "中文🙂", "🙂\r\n中", "\r\n", "尾", "absent"] {
            let expected: Vec<_> = memchr::memmem::find_iter(text.as_bytes(), pattern.as_bytes())
                .map(|offset| offset as u64..(offset + pattern.len()) as u64)
                .collect();
            for block in [1, 2, 3, 7, 64, 1024 * 1024] {
                for algorithm in [
                    SearchAlgorithm::MemchrBaseline,
                    SearchAlgorithm::ByteScanExperiment,
                ] {
                    let (actual, truncated, exact) =
                        matches_for(text, pattern, block, algorithm, 10_000);
                    assert_eq!(
                        actual, expected,
                        "text={text:?} pattern={pattern:?} block={block} algorithm={algorithm:?}"
                    );
                    assert!(!truncated);
                    assert!(exact);
                }
            }
        }
    }
}
#[test]
fn generated_search_cases_match_single_slice_reference() {
    let alphabet = ["a", "b", "中", "🙂", "\r\n"];
    let mut seed = 0x519ad12u64;
    for _ in 0..24 {
        let mut text = String::new();
        for _ in 0..73 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            text.push_str(alphabet[(seed >> 32) as usize % alphabet.len()]);
        }
        for pattern in ["a", "aa", "中🙂", "\r\na"] {
            let expected: Vec<_> = memchr::memmem::find_iter(text.as_bytes(), pattern.as_bytes())
                .map(|n| n as u64..(n + pattern.len()) as u64)
                .collect();
            for block in [1, 3, 7] {
                assert_eq!(
                    matches_for(
                        &text,
                        pattern,
                        block,
                        SearchAlgorithm::MemchrBaseline,
                        10_000
                    )
                    .0,
                    expected
                );
            }
        }
    }
}
#[test]
fn empty_pattern_and_cap_are_not_false_complete_counts() {
    assert_eq!(
        matches_for("abc", "", 1, SearchAlgorithm::MemchrBaseline, 10_000),
        (vec![], false, true)
    );
    let (matches, truncated, exact) = matches_for(
        &"a".repeat(20_000),
        "a",
        64,
        SearchAlgorithm::MemchrBaseline,
        10_000,
    );
    assert_eq!(matches.len(), 10_000);
    assert!(truncated);
    assert!(!exact);
}
#[test]
fn query_limit_and_unsupported_encoding_are_explicit() {
    let service = SearchService::with_limit(1);
    let target = SearchTarget::EditableSnapshot {
        doc_id: DocumentId::new(),
        rev: Revision(0),
        rope: Rope::new(),
    };
    assert!(
        service
            .start(SearchRequest::new(
                QueryId(1),
                vec![b'a'; 65_537],
                target.clone()
            ))
            .is_err()
    );
    assert!(
        service
            .start(SearchRequest::new(QueryId(1), vec![0xff], target))
            .is_err()
    );
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("utf16");
    fs::write(&path, [0xff, 0xfe, 0x61, 0]).unwrap();
    let doc = PagingService::default()
        .open(DocumentId::new(), path)
        .unwrap();
    let task = service
        .start(SearchRequest::new(
            QueryId(2),
            b"a".to_vec(),
            SearchTarget::PagedFile {
                source_gen: doc.source_generation(),
                handle: doc,
                encoding: EncodingSpec::UnsupportedReadOnly,
            },
        ))
        .unwrap();
    assert!(matches!(
        task.events.recv_timeout(Duration::from_secs(5)).unwrap(),
        SearchEvent::Failed {
            error: ServiceError {
                kind: ErrorKind::Unsupported,
                ..
            },
            ..
        }
    ));
}
#[test]
fn cancellation_exits_with_a_full_abandoned_result_queue() {
    let service = SearchService::with_limit(1);
    let request = SearchRequest::new(
        QueryId(4),
        b"a".to_vec(),
        SearchTarget::EditableSnapshot {
            doc_id: DocumentId::new(),
            rev: Revision(0),
            rope: Rope::from_str(&"a".repeat(2 * 1024 * 1024)),
        },
    );
    let task = service.start(request).unwrap();
    let waiting = Instant::now();
    while task.events.len() < 8 && waiting.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(task.events.len(), 8);
    let start = Instant::now();
    task.cancel.cancel();
    while service.active_tasks() != 0 && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        service.active_tasks(),
        0,
        "cancellation blocked behind full queue"
    );
}
#[test]
fn ordinary_open_preserves_bom_and_classifies_mixed_or_unsupported() {
    let dir = TempDir::new().unwrap();
    let paging = PagingService::default();
    for (bytes, ending, bom, readonly) in [
        (&b"alpha\nbeta\n"[..], LineEnding::Lf, false, false),
        (
            &b"\xef\xbb\xbfalpha\r\nbeta\r\n"[..],
            LineEnding::Crlf,
            true,
            false,
        ),
        (&b"a\r\nb\n"[..], LineEnding::MixedReadOnly, false, true),
        (&b"a\rb"[..], LineEnding::MixedReadOnly, false, true),
    ] {
        let path = dir.path().join(format!("file-{}", uuid::Uuid::new_v4()));
        fs::write(&path, bytes).unwrap();
        match admit(|| {
            open_document(
                DocumentId::new(),
                &path,
                &paging,
                OpenOptions::default(),
                &CancelToken::new(),
            )
        }) {
            OpenOutcome::Editable {
                rope,
                identity,
                encoding,
                line_ending,
                readonly_reason,
                ..
            } => {
                assert_eq!(line_ending, ending);
                assert_eq!(encoding == EncodingSpec::Utf8Bom, bom);
                assert_eq!(readonly_reason.is_some(), readonly);
                assert!(identity.matches_rope(&rope));
                assert_eq!(
                    rope.to_string().as_bytes(),
                    if bom { &bytes[3..] } else { bytes }
                );
            }
            other => panic!("unexpected route: {other:?}"),
        }
    }
    let path = dir.path().join("bad");
    fs::write(&path, [0xff, 0xfe, 0x61, 0]).unwrap();
    assert!(matches!(
        admit(|| open_document(
            DocumentId::new(),
            &path,
            &paging,
            OpenOptions::default(),
            &CancelToken::new()
        )),
        OpenOutcome::Paged {
            encoding: EncodingSpec::UnsupportedReadOnly,
            ..
        }
    ));
}
#[test]
fn ordinary_open_handles_utf8_crossing_scan_blocks() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("boundary");
    let text = format!("{}🙂\r\nend", "a\n".repeat((1024 * 1024 - 2) / 2));
    fs::write(&path, &text).unwrap();
    match admit(|| {
        open_document(
            DocumentId::new(),
            &path,
            &PagingService::default(),
            OpenOptions::default(),
            &CancelToken::new(),
        )
    }) {
        OpenOutcome::Editable { rope, .. } => assert_eq!(rope, text),
        other => panic!("unexpected route: {other:?}"),
    }
}
#[test]
fn paged_view_never_splits_utf8_or_crlf_and_long_lines_stay_bounded() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("text");
    let text = format!(
        "{}🙂\r\n{}",
        "x".repeat(65_533),
        "y".repeat(2 * 1024 * 1024)
    );
    fs::write(&path, &text).unwrap();
    let paging = PagingService::with_budgets(128 * 1024, 1024);
    let doc = paging.open(DocumentId::new(), &path).unwrap();
    for at in [
        0,
        65_533,
        65_534,
        65_536,
        65_538,
        128 * 1024,
        text.len() as u64,
    ] {
        let view = doc
            .viewport(
                ViewportRequest {
                    source_gen: doc.source_generation(),
                    anchor: ViewportAnchor::ByteOffset(at),
                    max_bytes: 65_536,
                },
                &CancelToken::new(),
            )
            .unwrap();
        assert!(view.text.len() <= 65_536);
        assert!(!view.lossy);
        assert!(text.is_char_boundary(view.byte_range.start as usize));
        assert!(text.is_char_boundary(view.byte_range.end as usize));
        assert_eq!(
            &text[view.byte_range.start as usize..view.byte_range.end as usize],
            view.text.as_ref()
        );
        if view.byte_range.start > 0 && !view.text.is_empty() {
            assert!(
                !(text.as_bytes()[view.byte_range.start as usize - 1] == b'\r'
                    && view.text.starts_with('\n'))
            );
        }
        if view.byte_range.end < text.len() as u64 {
            assert!(
                !(view.text.ends_with('\r')
                    && text.as_bytes()[view.byte_range.end as usize] == b'\n')
            );
        }
    }
    let view = doc
        .viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(500_000),
                max_bytes: 65_536,
            },
            &CancelToken::new(),
        )
        .unwrap();
    assert!(view.is_line_fragment);
    assert!(paging.stats().cached_bytes <= 128 * 1024);
}
#[test]
fn all_documents_share_cache_and_dense_index_budgets() {
    let dir = TempDir::new().unwrap();
    let paging = PagingService::with_budgets(4 * 65_536, 1024);
    let mut docs = Vec::new();
    for n in 0..16 {
        let path = dir.path().join(format!("{n}"));
        fs::write(&path, vec![b'\n'; 512 * 1024]).unwrap();
        let doc = paging.open(DocumentId::new(), path).unwrap();
        doc.viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(123),
                max_bytes: 65_536,
            },
            &CancelToken::new(),
        )
        .unwrap();
        assert!(matches!(
            admit(|| doc.index_step(doc.source_generation(), &CancelToken::new())),
            IndexStatus::Complete {
                total_lines: 524_289,
                ..
            }
        ));
        docs.push(doc);
    }
    for doc in &docs {
        assert!(
            doc.index_checkpoint_count() > 0,
            "a later file must receive fair sparse-index capacity"
        );
        let view = doc
            .viewport(
                ViewportRequest {
                    source_gen: doc.source_generation(),
                    anchor: ViewportAnchor::ByteOffset(96 * 1024),
                    max_bytes: 64,
                },
                &CancelToken::new(),
            )
            .unwrap();
        assert_eq!(view.first_line_number, Some(96 * 1024));
    }
    assert!(paging.stats().cached_bytes <= paging.stats().cache_budget);
    assert!(paging.stats().index_bytes <= paging.stats().index_budget);
    drop(docs);
    assert_eq!(paging.stats().cached_bytes, 0);
    assert_eq!(paging.stats().index_bytes, 0);
    assert_eq!(paging.stats().sources, 0);
}
#[test]
fn source_identity_invalidates_replacement_truncation_append_and_rewrite() {
    for mutation in 0..4 {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("source");
        fs::write(&path, b"original text\n").unwrap();
        let paging = PagingService::with_budgets(65_536, 1024);
        let doc = paging.open(DocumentId::new(), &path).unwrap();
        let generation = doc.source_generation();
        doc.viewport(
            ViewportRequest {
                source_gen: generation,
                anchor: ViewportAnchor::ByteOffset(0),
                max_bytes: 64,
            },
            &CancelToken::new(),
        )
        .unwrap();
        match mutation {
            0 => {
                let replacement = dir.path().join("replacement");
                fs::write(&replacement, b"replacement\n").unwrap();
                fs::rename(replacement, &path).unwrap();
            }
            1 => {
                fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(2)
                    .unwrap();
            }
            2 => {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(b"append")
                    .unwrap();
            }
            _ => fs::write(&path, b"different text sameish\n").unwrap(),
        }
        assert_eq!(
            doc.validate(generation).unwrap_err().kind,
            ErrorKind::SourceChanged
        );
        assert_ne!(doc.source_generation(), generation);
        assert_eq!(paging.stats().cached_bytes, 0);
    }
}
#[test]
fn recovery_baseline_deltas_gap_compaction_and_external_source_loss() {
    let dir = TempDir::new().unwrap();
    let source = dir.path().join("source");
    fs::write(&source, "original").unwrap();
    let database = dir.path().join("recovery.db");
    let service = RecoveryHandle::start(&database).unwrap();
    let id = DocumentId::new();
    let mut first = snapshot(id, 1, "中文🙂");
    first.meta.path = Some(source.clone());
    first.meta.origin_fingerprint = Some(FileFingerprint::of_path(&source).unwrap());
    admit(|| service.try_commit(first.clone()));
    receive(service.flush().unwrap());
    let mut second = snapshot(id, 2, "中文🙂!");
    second.meta = first.meta.clone();
    second.deltas = vec![TextDelta {
        byte_range: 10..10,
        replacement: "!".into(),
    }];
    admit(|| service.try_commit(second.clone()));
    receive(service.flush().unwrap());
    let conn = rusqlite::Connection::open(&database).unwrap();
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM edit_transactions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        rows, 1,
        "ordinary typing must append deltas, not full snapshots"
    );
    fs::remove_file(source).unwrap();
    let hydrated = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(hydrated.rope, "中文🙂!");
    assert!(hydrated.disk_conflict);
    assert_eq!(hydrated.durable_rev, Revision(2));
    conn.execute("UPDATE recovery_heads SET delta_rows=511", [])
        .unwrap();
    let mut compact = snapshot(id, 3, "中文🙂!!");
    compact.deltas = vec![TextDelta {
        byte_range: 11..11,
        replacement: "!".into(),
    }];
    admit(|| service.try_commit(compact.clone()));
    receive(service.flush().unwrap());
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .rope,
        "中文🙂!!"
    );
    let checkpoint_rev: i64 = conn
        .query_row("SELECT checkpoint_rev FROM recovery_heads", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        checkpoint_rev, 3,
        "row threshold must promote to a guarded checkpoint"
    );
    let mut gap = snapshot(id, 7, "self-contained after queue gap");
    gap.base_rev = Revision(6);
    gap.deltas = vec![TextDelta {
        byte_range: 0..0,
        replacement: "irrelevant when baseline is missing".into(),
    }];
    admit(|| service.try_commit(gap.clone()));
    receive(service.flush().unwrap());
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .rope,
        "self-contained after queue gap"
    );
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM edit_transactions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
    let late = snapshot(id, 3, "late old snapshot");
    admit(|| service.try_commit(late.clone()));
    receive(service.flush().unwrap());
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .durable_rev,
        Revision(7)
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn invalid_unicode_delta_does_not_advance_durable_revision() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let id = DocumentId::new();
    let baseline = snapshot(id, 1, "中");
    admit(|| service.try_commit(baseline.clone()));
    receive(service.flush().unwrap());
    let mut invalid = snapshot(id, 2, "xx");
    invalid.deltas = vec![TextDelta {
        byte_range: 1..2,
        replacement: String::new(),
    }];
    admit(|| service.try_commit(invalid.clone()));
    let result = service
        .flush()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    assert!(result.is_err());
    // A malformed caller submission must not receive an acknowledgement for invalid deltas.
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .durable_rev,
        Revision(1)
    );
    // Supply an authoritative replacement snapshot explicitly to repair this rejected input.
    let repaired = snapshot(id, 3, "repaired");
    admit(|| service.try_commit(repaired.clone()));
    let _ = service
        .flush()
        .unwrap()
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    receive(admit(|| service.shutdown()));
}
#[test]
fn discard_tombstone_rejects_late_snapshots_and_close_keeps_recent_draft() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let id = DocumentId::new();
    let baseline = snapshot(id, 1, "keep recently closed");
    admit(|| service.try_commit(baseline.clone()));
    receive(service.flush().unwrap());
    let mut layout = SessionLayout::default();
    let target = add_layout_view(&mut layout, document_key(id), ViewState::default(), true);
    persist_test_layout(
        &service,
        &layout,
        vec![DocumentChange::Create(DocumentMetaRecord {
            key: target.document,
            meta: baseline.meta.clone(),
        })],
    );
    close_test_view(
        &service,
        target,
        CloseDisposition::KeepRecovery,
        Some(Revision(1)),
    );
    assert_eq!(
        receive(service.request_recently_closed(None, 64).unwrap())
            .entries
            .len(),
        1
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(target.document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "keep recently closed"
    );
    let mut reopened = receive(service.request_manifest().unwrap()).layout;
    let fresh_view = add_layout_view(&mut reopened, target.document, ViewState::default(), true);
    assert_ne!(fresh_view.view_id, target.view_id);
    persist_test_layout(
        &service,
        &reopened,
        vec![DocumentChange::Reopen(target.document)],
    );
    close_test_view(
        &service,
        fresh_view,
        CloseDisposition::Discard {
            tombstone_gen: LifecycleGen(2),
        },
        Some(Revision(1)),
    );
    let late = snapshot(id, 2, "late resurrection");
    admit(|| service.try_commit(late.clone()));
    receive(service.flush().unwrap());
    assert!(
        service
            .request_hydrate(target.document, CancelToken::new())
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err()
    );
    assert!(
        receive(service.request_recently_closed(None, 64).unwrap())
            .entries
            .is_empty()
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn session_manifest_is_metadata_only_and_corrupt_draft_is_isolated() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("db");
    let service = RecoveryHandle::start(&database).unwrap();
    let mut layout = SessionLayout::default();
    let mut targets = Vec::new();
    let mut changes = Vec::new();
    for n in 0..100 {
        let id = DocumentId::new();
        let mut job = snapshot(id, 1, &format!("draft {n} 中文🙂"));
        job.meta.title = format!("Draft {n}");
        let target = add_layout_view(
            &mut layout,
            document_key(id),
            ViewState {
                cursor_byte: n,
                scroll_byte: n * 2,
            },
            n == 42,
        );
        targets.push(target);
        changes.push(DocumentChange::Create(DocumentMetaRecord {
            key: target.document,
            meta: job.meta.clone(),
        }));
        admit(|| service.try_commit(job.clone()));
    }
    persist_test_layout(&service, &layout, changes);
    receive(admit(|| service.flush()));
    let mut durable = std::collections::HashMap::new();
    for event in service.events().try_iter() {
        match event {
            RecoveryEvent::DurableAck {
                doc_id,
                lifecycle_gen,
                durable_rev,
            } => {
                assert_eq!(lifecycle_gen, LifecycleGen(1));
                durable.insert(doc_id, durable_rev);
            }
            RecoveryEvent::StorageError { error, .. } => {
                panic!("unexpected persistence failure: {error}")
            }
        }
    }
    assert_eq!(
        durable.len(),
        100,
        "every retained submission receives a durable acknowledgment"
    );
    for target in &targets {
        assert_eq!(durable.get(&target.document.doc_id), Some(&Revision(1)));
    }
    let conn = rusqlite::Connection::open(database).unwrap();
    conn.execute(
        "UPDATE checkpoint_chunks SET content=x'ff' WHERE doc=?1",
        [format!("{:032x}", targets[99].document.doc_id.0)],
    )
    .unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.documents.len(), 100);
    assert_eq!(
        manifest.layout.pane().active_view,
        Some(targets[42].view_id)
    );
    assert_eq!(
        manifest.layout.views[&targets[42].view_id].view.cursor_byte,
        42
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(targets[42].document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "draft 42 中文🙂"
    );
    assert!(
        service
            .request_hydrate(targets[99].document, CancelToken::new())
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err()
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(targets[0].document, CancelToken::new())
                .unwrap()
        )
        .rope,
        "draft 0 中文🙂"
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn saved_association_survives_later_commit_with_captured_old_metadata() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let id = DocumentId::new();
    let mut baseline = snapshot(id, 1, "saved");
    // The user has already changed the desired format/language while this save runs.
    baseline.meta.encoding = EncodingSpec::Utf8Bom;
    baseline.meta.line_ending = LineEnding::Crlf;
    baseline.meta.language_override = Some("markdown".into());
    admit(|| service.try_commit(baseline.clone()));
    receive(service.flush().unwrap());
    let identity = ContentIdentity::of_rope(&Rope::from_str("saved"));
    let path = dir.path().join("saved.txt");
    fs::write(&path, "saved").unwrap();
    receive(
        service
            .record_save_intent(SaveIntent {
                doc_id: id,
                lifecycle_gen: LifecycleGen(1),
                captured_rev: Revision(1),
                target_path: path.clone(),
                captured_identity: identity,
                captured_encoding: Some(EncodingSpec::Utf8),
                captured_line_ending: Some(LineEnding::Lf),
                stage: SaveStage::Published,
                output_fingerprint: Some(FileFingerprint::of_path(&path).unwrap()),
            })
            .unwrap(),
    );
    let published = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(published.meta.encoding, EncodingSpec::Utf8Bom);
    assert_eq!(published.meta.line_ending, LineEnding::Crlf);
    assert_eq!(
        published.meta.language_override.as_deref(),
        Some("markdown")
    );
    assert_eq!(published.meta.saved_encoding, Some(EncodingSpec::Utf8));
    assert_eq!(published.meta.saved_line_ending, Some(LineEnding::Lf));
    let mut latest = snapshot(id, 2, "saved plus later typing");
    latest.meta.encoding = EncodingSpec::Utf8Bom;
    latest.meta.line_ending = LineEnding::Crlf;
    latest.meta.language_override = Some("rust".into());
    admit(|| service.try_commit(latest.clone()));
    // A newer non-dirty layout may be applied before this still-batched text commit.
    // Do not flush between these admissions: that would hide the metadata ordering race.
    latest.meta.encoding = EncodingSpec::Utf8;
    latest.meta.language_override = Some("python".into());
    let mut layout = SessionLayout::default();
    add_layout_view(&mut layout, document_key(id), ViewState::default(), true);
    persist_test_layout(
        &service,
        &layout,
        vec![DocumentChange::Create(DocumentMetaRecord {
            key: document_key(id),
            meta: latest.meta,
        })],
    );
    receive(admit(|| service.flush()));
    let restored = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(restored.meta.path, Some(path));
    assert_eq!(restored.meta.saved_encoding, Some(EncodingSpec::Utf8));
    assert_eq!(restored.meta.saved_line_ending, Some(LineEnding::Lf));
    assert_eq!(restored.meta.encoding, EncodingSpec::Utf8);
    assert_eq!(restored.meta.line_ending, LineEnding::Crlf);
    assert_eq!(restored.meta.language_override.as_deref(), Some("python"));
    assert_eq!(restored.meta.saved_identity, Some(identity));
    assert_eq!(restored.rope, "saved plus later typing");
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.documents.len(), 1);
    assert_eq!(
        manifest.documents[0].meta.language_override.as_deref(),
        Some("python")
    );
    assert_eq!(manifest.documents[0].meta.encoding, EncodingSpec::Utf8);
    assert_eq!(manifest.documents[0].meta.line_ending, LineEnding::Crlf);
    assert_eq!(
        manifest.documents[0].meta.saved_encoding,
        Some(EncodingSpec::Utf8)
    );
    assert_eq!(
        manifest.documents[0].meta.saved_line_ending,
        Some(LineEnding::Lf)
    );
    receive(admit(|| service.shutdown()));
}
#[test]
fn clean_identity_ignores_revision_and_handles_undo_back_to_savepoint() {
    let saved = Rope::from_str("中文🙂 saved");
    let identity = ContentIdentity::of_rope(&saved);
    let mut changed = saved.clone();
    changed.insert(changed.len(), "more");
    assert!(!identity.matches_rope(&changed));
    changed.remove(saved.len()..changed.len());
    assert!(identity.matches_rope(&changed));
    changed.remove(0..3);
    changed.insert(0, "字");
    assert!(!identity.matches_rope(&changed));
}
#[test]
fn process_exit_before_sqlite_commit_recovers_only_prior_complete_revision() {
    let dir = TempDir::new().unwrap();
    let binary = env!("CARGO_BIN_EXE_editor-services");
    let status = std::process::Command::new(binary)
        .args(["crash-recovery", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
    let status = std::process::Command::new(binary)
        .args(["recover-check", dir.path().to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(target_os = "macos")]
fn save_request(path: &Path, text: &str) -> SaveRequest {
    SaveRequest {
        doc_id: DocumentId::new(),
        lifecycle_gen: LifecycleGen(1),
        captured_rev: Revision(7),
        rope: Rope::from_str(text),
        target_path: path.to_path_buf(),
        encoding: EncodingSpec::Utf8,
        line_ending: LineEnding::Lf,
        expected_fingerprint: FileFingerprint::of_path(path).ok(),
        force_overwrite: false,
    }
}
#[cfg(target_os = "macos")]
#[test]
fn captured_save_normalizes_eol_preserves_bom_and_rescues_without_database() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("crlf");
    fs::write(&path, b"\xef\xbb\xbfline\r\n").unwrap();
    let mut request = save_request(&path, "line\r\nnew\n");
    request.encoding = EncodingSpec::Utf8Bom;
    request.line_ending = LineEnding::Crlf;
    let outcome = save_document(&request, None, &FaultInjector::default());
    assert!(
        matches!(
            outcome,
            SaveOutcome::SavedWithRecoveryWarning {
                captured_rev: Revision(7),
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfline\r\nnew\r\n");
    let captured = request.rope.clone();
    request.rope.insert(request.rope.len(), "later typing");
    assert_ne!(request.rope, captured);
    let path = dir.path().join("lf");
    let request = save_request(&path, "line\r\nnew\n");
    assert!(matches!(
        save_document(&request, None, &FaultInjector::default()),
        SaveOutcome::SavedWithRecoveryWarning { .. }
    ));
    assert_eq!(fs::read(path).unwrap(), b"line\nnew\n");
}
#[cfg(target_os = "macos")]
#[test]
fn every_prepublication_save_fault_preserves_original_and_cleans_temp() {
    for point in [
        FaultPoint::SaveBeforeWrite,
        FaultPoint::SaveAfterTempSync,
        FaultPoint::SaveBeforeRename,
    ] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("target");
        fs::write(&path, b"original").unwrap();
        let request = save_request(&path, "replacement");
        let faults = FaultInjector::default();
        faults.arm(point);
        assert!(matches!(
            save_document(&request, None, &faults),
            SaveOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), b"original");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
#[cfg(target_os = "macos")]
#[test]
fn publication_and_recovery_warning_are_not_reported_as_no_file_saved() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("target");
    fs::write(&path, b"original").unwrap();
    let faults = FaultInjector::default();
    faults.arm(FaultPoint::SaveAfterRename);
    assert!(matches!(
        save_document(&save_request(&path, "published"), None, &faults),
        SaveOutcome::PublishedNotDurable { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), b"published");
    faults.arm(FaultPoint::SaveBeforeRecoveryAck);
    assert!(matches!(
        save_document(&save_request(&path, "published again"), None, &faults),
        SaveOutcome::SavedWithRecoveryWarning { .. }
    ));
    assert_eq!(fs::read(path).unwrap(), b"published again");
}
#[cfg(target_os = "macos")]
#[test]
fn save_detects_conflicts_and_preserves_symlink_target_and_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("target");
    fs::write(&path, b"original").unwrap();
    let request = save_request(&path, "new");
    fs::write(&path, b"external changed file").unwrap();
    assert!(matches!(
        save_document(&request, None, &FaultInjector::default()),
        SaveOutcome::ConflictDetected { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), b"external changed file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    let link = dir.path().join("symlink");
    symlink(&path, &link).unwrap();
    assert!(matches!(
        save_document(
            &save_request(&link, "via symlink"),
            None,
            &FaultInjector::default()
        ),
        SaveOutcome::SavedWithRecoveryWarning { .. }
    ));
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(&path).unwrap(), b"via symlink");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o640
    );
    let hardlink = dir.path().join("hardlink");
    fs::hard_link(&path, &hardlink).unwrap();
    assert!(matches!(
        save_document(
            &save_request(&path, "would break link"),
            None,
            &FaultInjector::default()
        ),
        SaveOutcome::Failed {
            error: ServiceError {
                kind: ErrorKind::Unsupported,
                ..
            },
            ..
        }
    ));
    assert_eq!(fs::read(hardlink).unwrap(), b"via symlink");
}
#[cfg(target_os = "macos")]
#[test]
fn save_process_exit_stages_keep_either_complete_old_or_complete_new_file() {
    let binary = env!("CARGO_BIN_EXE_editor-services");
    for (stage, expected) in [
        ("before-write", "original\n"),
        ("temp-sync", "original\n"),
        ("before-rename", "original\n"),
        ("after-rename", "captured new text\n"),
        ("recovery-ack", "captured new text\n"),
    ] {
        let dir = TempDir::new().unwrap();
        let status = std::process::Command::new(binary)
            .args(["crash-save", dir.path().to_str().unwrap(), stage])
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "stage={stage}");
        assert_eq!(
            fs::read_to_string(dir.path().join("crash-save.txt")).unwrap(),
            expected
        );
        let service = RecoveryHandle::start(dir.path().join("save.sqlite3")).unwrap();
        assert_eq!(
            receive(
                service
                    .request_hydrate(document_key(DocumentId(0x5a7e)), CancelToken::new())
                    .unwrap()
            )
            .rope,
            "captured new text\n"
        );
        let intents = receive(service.request_save_intents().unwrap());
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].target_matches_prepared_output().unwrap(),
            matches!(stage, "after-rename" | "recovery-ack")
        );
        receive(admit(|| service.shutdown()));
    }
}

#[test]
fn lossy_viewport_remains_bounded_and_reports_consumed_raw_bytes() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("invalid");
    fs::write(&path, vec![0x80; 256 * 1024]).unwrap();
    let doc = PagingService::default()
        .open(DocumentId::new(), path)
        .unwrap();
    let view = doc
        .viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(0),
                max_bytes: 65_536,
            },
            &CancelToken::new(),
        )
        .unwrap();
    assert!(view.lossy);
    assert!(!view.text.is_empty());
    assert!(view.text.len() <= 65_536);
    assert_eq!(
        view.text.chars().count() as u64,
        view.byte_range.end - view.byte_range.start
    );
}

#[cfg(target_os = "macos")]
#[test]
fn unmodified_supported_files_save_byte_identically() {
    let dir = TempDir::new().unwrap();
    for (n, bytes) in [
        &b"alpha\nbeta\n"[..],
        &b"\xef\xbb\xbfalpha\nbeta\n"[..],
        &b"alpha\r\nbeta\r\n"[..],
        &b"\xef\xbb\xbfalpha\r\nbeta\r\n"[..],
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(n.to_string());
        fs::write(&path, bytes).unwrap();
        let id = DocumentId::new();
        let OpenOutcome::Editable {
            rope,
            encoding,
            line_ending,
            fingerprint,
            ..
        } = admit(|| {
            open_document(
                id,
                &path,
                &PagingService::default(),
                OpenOptions::default(),
                &CancelToken::new(),
            )
        })
        else {
            panic!("unexpected read-only routing");
        };
        let request = SaveRequest {
            doc_id: id,
            lifecycle_gen: LifecycleGen(1),
            captured_rev: Revision(0),
            rope,
            target_path: path.clone(),
            encoding,
            line_ending,
            expected_fingerprint: Some(fingerprint),
            force_overwrite: false,
        };
        assert!(matches!(
            save_document(&request, None, &FaultInjector::default()),
            SaveOutcome::SavedWithRecoveryWarning { .. }
        ));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn rapid_search_supersession_keeps_latest_query_in_one_bounded_slot() {
    for service in [SearchService::with_limit(1), SearchService::default()] {
        let id = DocumentId::new();
        let text = format!("{}needle-final", "a".repeat(2 * 1024 * 1024));
        let rope = Rope::from_str(&text);
        let mut tasks = Vec::new();
        for query in 1..=20 {
            let pattern = if query == 20 { "needle-final" } else { "a" };
            tasks.push(
                service
                    .start(SearchRequest::new(
                        QueryId(query),
                        pattern.as_bytes().to_vec(),
                        SearchTarget::EditableSnapshot {
                            doc_id: id,
                            rev: Revision(1),
                            rope: rope.clone(),
                        },
                    ))
                    .expect("same-document replacement cannot be Busy"),
            );
        }
        assert!(service.active_tasks() <= 1);
        let latest = tasks.last().unwrap();
        let mut matches = Vec::new();
        loop {
            match latest.events.recv_timeout(Duration::from_secs(10)).unwrap() {
                SearchEvent::Batch {
                    query_id,
                    matches: batch,
                } => {
                    assert_eq!(query_id, QueryId(20));
                    matches.extend(batch);
                }
                SearchEvent::Done {
                    query_id,
                    total_emitted,
                    truncated,
                    count_exact,
                    ..
                } => {
                    assert_eq!(query_id, QueryId(20));
                    assert_eq!(total_emitted, 1);
                    assert!(!truncated);
                    assert!(count_exact);
                    break;
                }
                event => panic!("newest search was lost: {event:?}"),
            }
        }
        assert_eq!(
            matches[0].byte_range,
            (text.len() - "needle-final".len()) as u64..text.len() as u64
        );
        let settling = Instant::now();
        while service.active_tasks() != 0 && settling.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(service.active_tasks(), 0);
    }
}

#[test]
fn capped_search_reports_scanned_cursor_and_read_bytes_separately() {
    for length in [10_000, 10_001, 1_048_576] {
        let request = SearchRequest::new(
            QueryId(1),
            b"a".to_vec(),
            SearchTarget::EditableSnapshot {
                doc_id: DocumentId::new(),
                rev: Revision(0),
                rope: Rope::from_str(&"a".repeat(length)),
            },
        );
        let service = SearchService::with_limit(1);
        let task = service.start(request).unwrap();
        loop {
            match task.events.recv_timeout(Duration::from_secs(10)).unwrap() {
                SearchEvent::Batch { .. } => {}
                SearchEvent::Done {
                    total_emitted,
                    truncated,
                    count_exact,
                    bytes_scanned,
                    bytes_read,
                    ..
                } => {
                    assert_eq!(total_emitted, 10_000);
                    assert!(
                        truncated,
                        "reaching the cap is incomplete even exactly at EOF"
                    );
                    assert!(!count_exact);
                    assert_eq!(bytes_scanned, 10_000);
                    assert_eq!(bytes_read, length as u64);
                    break;
                }
                event => panic!("unexpected search event: {event:?}"),
            }
        }
    }
}

#[test]
fn generations_are_unique_per_open_and_advance_on_invalidation() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("source");
    fs::write(&path, vec![b'x'; 128 * 1024]).unwrap();
    let paging = PagingService::with_budgets(128 * 1024, 1024);
    let first = paging.open(DocumentId::new(), &path).unwrap();
    let second = paging.open(first.doc_id(), &path).unwrap();
    let original = first.source_generation();
    assert_ne!(original, second.source_generation());
    first
        .viewport(
            ViewportRequest {
                source_gen: original,
                anchor: ViewportAnchor::ByteOffset(0),
                max_bytes: 64,
            },
            &CancelToken::new(),
        )
        .unwrap();
    fs::write(&path, b"short").unwrap();
    assert_eq!(
        first.validate(original).unwrap_err().kind,
        ErrorKind::SourceChanged
    );
    assert!(first.source_generation().0 > original.0);
    assert_eq!(paging.stats().cached_bytes, 0);
    let reopened = paging.open(first.doc_id(), path).unwrap();
    assert!(reopened.source_generation().0 > first.source_generation().0);
    assert!(
        first
            .viewport(
                ViewportRequest {
                    source_gen: original,
                    anchor: ViewportAnchor::ByteOffset(0),
                    max_bytes: 64
                },
                &CancelToken::new()
            )
            .is_err()
    );
    assert_eq!(paging.stats().cached_bytes, 0);
}

#[test]
fn lossy_decoding_preserves_crlf_and_exact_requested_binary_anchor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("crlf");
    fs::write(&path, [0xff, b'a', b'b', b'c', b'd', b'\r', b'\n', b'x']).unwrap();
    let doc = PagingService::default()
        .open(DocumentId::new(), path)
        .unwrap();
    let view = doc
        .viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(0),
                max_bytes: 8,
            },
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(view.byte_range, 0..5);
    assert!(view.lossy);
    assert!(!view.text.ends_with('\r'));
    let path = dir.path().join("continuations");
    fs::write(&path, vec![0x80; 256 * 1024]).unwrap();
    let doc = PagingService::default()
        .open(DocumentId::new(), path)
        .unwrap();
    let view = doc
        .viewport(
            ViewportRequest {
                source_gen: doc.source_generation(),
                anchor: ViewportAnchor::ByteOffset(65_536),
                max_bytes: 65_536,
            },
            &CancelToken::new(),
        )
        .unwrap();
    assert_eq!(view.byte_range.start, 65_536);
    assert!(view.lossy);
    assert!(view.byte_range.end > view.byte_range.start);
}

#[test]
fn opening_keeps_binary_priority_and_equal_lf_crlf_long_line_thresholds() {
    let dir = TempDir::new().unwrap();
    let paging = PagingService::default();
    for (number, bytes) in [
        vec![0x89, b'P', b'N', b'G', 0, 0xff],
        vec![0; 2 * 1024 * 1024],
    ]
    .into_iter()
    .enumerate()
    {
        let path = dir.path().join(format!("binary-{number}"));
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            admit(|| open_document(
                DocumentId::new(),
                &path,
                &paging,
                OpenOptions::default(),
                &CancelToken::new()
            )),
            OpenOutcome::Paged {
                readonly_reason: ReadOnlyReason::BinaryContent,
                encoding: EncodingSpec::UnsupportedReadOnly,
                ..
            }
        ));
    }
    for ending in ["\n", "\r\n"] {
        let path = dir.path().join(uuid::Uuid::new_v4().to_string());
        let text = format!("{}{ending}", "a".repeat(1_048_576));
        fs::write(&path, &text).unwrap();
        let OpenOutcome::Editable { rope, identity, .. } = admit(|| {
            open_document(
                DocumentId::new(),
                &path,
                &paging,
                OpenOptions::default(),
                &CancelToken::new(),
            )
        }) else {
            panic!("LF and CRLF must share the same content-byte threshold");
        };
        assert_eq!(rope, text);
        assert_eq!(identity, ContentIdentity::of_rope(&rope));
    }
}

#[test]
fn concurrent_open_and_index_requests_retry_without_exceeding_global_quotas() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ordinary");
    fs::write(&path, "line 中文🙂\n".repeat(32_768)).unwrap();
    let paging = PagingService::default();
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for _ in 0..16 {
            let path = &path;
            let paging = &paging;
            workers.push(scope.spawn(move || {
                let id = DocumentId::new();
                let opened = admit(|| {
                    open_document(
                        id,
                        path,
                        paging,
                        OpenOptions::default(),
                        &CancelToken::new(),
                    )
                });
                assert!(matches!(opened, OpenOutcome::Editable { .. }));
                let doc = paging.open(id, path).unwrap();
                loop {
                    if matches!(
                        admit(|| doc.index_step(doc.source_generation(), &CancelToken::new())),
                        IndexStatus::Complete { .. }
                    ) {
                        break;
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let stats = paging.stats();
    assert!(stats.peak_open_jobs <= 2);
    assert!(stats.peak_index_jobs <= 2);
    assert!(stats.peak_open_jobs > 0);
    assert!(stats.peak_index_jobs > 0);
}

#[test]
fn durable_head_without_saved_layout_is_visible_after_restart() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("db");
    let id = DocumentId::new();
    let service = RecoveryHandle::start(&database).unwrap();
    let job = snapshot(id, 1, "durable without layout 中文🙂");
    admit(|| service.try_commit(job.clone()));
    receive(admit(|| service.flush()));
    receive(admit(|| service.shutdown()));
    drop(service);
    let reopened = RecoveryHandle::start(database).unwrap();
    let manifest = receive(reopened.request_manifest().unwrap());
    assert!(manifest.warnings.is_empty());
    assert_eq!(manifest.documents.len(), 1);
    assert_eq!(manifest.documents[0].doc_id, id);
    assert_eq!(
        receive(
            reopened
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .rope,
        "durable without layout 中文🙂"
    );
    let layout = manifest.layout;
    let target = layout.target(layout.pane().tabs[0]).unwrap();
    persist_test_layout(&reopened, &layout, Vec::new());
    close_test_view(
        &reopened,
        target,
        CloseDisposition::Discard {
            tombstone_gen: LifecycleGen(2),
        },
        Some(Revision(1)),
    );
    assert!(
        receive(reopened.request_manifest().unwrap())
            .documents
            .is_empty()
    );
    receive(admit(|| reopened.shutdown()));
}

#[test]
fn malformed_metadata_and_view_are_isolated_with_visible_manifest_warnings() {
    let dir = TempDir::new().unwrap();
    let database = dir.path().join("db");
    let service = RecoveryHandle::start(&database).unwrap();
    let ids = [DocumentId::new(), DocumentId::new(), DocumentId::new()];
    let mut layout = SessionLayout::default();
    let mut changes = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let job = snapshot(*id, 1, &format!("intact draft {index}"));
        add_layout_view(
            &mut layout,
            document_key(*id),
            ViewState {
                cursor_byte: 7,
                scroll_byte: 8,
            },
            index == 0,
        );
        changes.push(DocumentChange::Create(DocumentMetaRecord {
            key: document_key(*id),
            meta: job.meta.clone(),
        }));
        admit(|| service.try_commit(job.clone()));
    }
    persist_test_layout(&service, &layout, changes);
    receive(admit(|| service.flush()));
    let conn = rusqlite::Connection::open(database).unwrap();
    conn.execute(
        "UPDATE documents SET meta='invalid json' WHERE id=?1",
        [format!("{:032x}", ids[1].0)],
    )
    .unwrap();
    conn.execute(
        "UPDATE layout_views SET view_state='invalid json' WHERE doc=?1",
        [format!("{:032x}", ids[2].0)],
    )
    .unwrap();
    let manifest = receive(service.request_manifest().unwrap());
    assert_eq!(manifest.documents.len(), 2);
    assert!(manifest.documents.iter().any(|stub| stub.doc_id == ids[0]));
    let reset_view = manifest
        .layout
        .views
        .values()
        .find(|view| view.document.doc_id == ids[2])
        .unwrap();
    assert_eq!(reset_view.view, ViewState::default());
    let catalog = receive(
        service
            .request_recovery_page(RecoveryPageRequest {
                cursor: None,
                limit: 64,
            })
            .unwrap(),
    );
    assert!(
        catalog
            .entries
            .iter()
            .any(|entry| entry.key.doc_id == Some(ids[1]) && entry.stub.is_err())
    );
    assert!(
        manifest
            .warnings
            .iter()
            .any(|warning| warning.doc_id.is_none() && warning.message.contains("layout"))
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(ids[0]), CancelToken::new())
                .unwrap()
        )
        .rope,
        "intact draft 0"
    );
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(ids[2]), CancelToken::new())
                .unwrap()
        )
        .rope,
        "intact draft 2"
    );
    let preserved: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM checkpoint_chunks WHERE doc=?1",
            [format!("{:032x}", ids[1].0)],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        preserved > 0,
        "isolation must not delete the damaged draft's intact text"
    );
    receive(admit(|| service.shutdown()));
}

#[test]
fn valid_but_inaccurate_equal_length_deltas_checkpoint_the_captured_rope() {
    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let id = DocumentId::new();
    let baseline = snapshot(id, 1, "ab");
    admit(|| service.try_commit(baseline.clone()));
    receive(admit(|| service.flush()));
    let mut mismatched = snapshot(id, 2, "ab");
    mismatched.deltas = vec![TextDelta {
        byte_range: 0..1,
        replacement: "X".into(),
    }];
    admit(|| service.try_commit(mismatched.clone()));
    receive(admit(|| service.flush()));
    let restored = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(restored.durable_rev, Revision(2));
    assert_eq!(restored.rope, "ab");
    let mut next = snapshot(id, 3, "abc");
    next.deltas = vec![TextDelta {
        byte_range: 2..2,
        replacement: "c".into(),
    }];
    admit(|| service.try_commit(next.clone()));
    receive(admit(|| service.flush()));
    assert_eq!(
        receive(
            service
                .request_hydrate(document_key(id), CancelToken::new())
                .unwrap()
        )
        .rope,
        "abc"
    );
    receive(admit(|| service.shutdown()));
}

#[test]
fn legacy_metadata_and_save_intents_keep_unknown_format_baselines() {
    let identity = ContentIdentity {
        byte_len: 5,
        hash128: 7,
    };
    let meta = DocMeta {
        path: Some("legacy.txt".into()),
        encoding: EncodingSpec::Utf8Bom,
        line_ending: LineEnding::Crlf,
        saved_identity: Some(identity),
        ..DocMeta::default()
    };
    let mut json = serde_json::to_value(meta).unwrap();
    for field in ["saved_encoding", "saved_line_ending", "language_override"] {
        json.as_object_mut().unwrap().remove(field);
    }
    let legacy: DocMeta = serde_json::from_value(json).unwrap();
    assert_eq!(legacy.saved_encoding, None);
    assert_eq!(legacy.saved_line_ending, None);
    assert_eq!(legacy.language_override, None);
    assert!(legacy.saved_identity.is_some());
    assert_ne!(legacy.saved_encoding, Some(legacy.encoding));
    assert_ne!(legacy.saved_line_ending, Some(legacy.line_ending));

    let intent = SaveIntent {
        doc_id: DocumentId(11),
        lifecycle_gen: LifecycleGen(1),
        captured_rev: Revision(1),
        target_path: "legacy.txt".into(),
        captured_identity: identity,
        captured_encoding: Some(EncodingSpec::Utf8Bom),
        captured_line_ending: Some(LineEnding::Crlf),
        stage: SaveStage::Published,
        output_fingerprint: None,
    };
    let mut json = serde_json::to_value(intent).unwrap();
    json.as_object_mut().unwrap().remove("captured_encoding");
    json.as_object_mut().unwrap().remove("captured_line_ending");
    let legacy: SaveIntent = serde_json::from_value(json).unwrap();
    assert_eq!(legacy.captured_encoding, None);
    assert_eq!(legacy.captured_line_ending, None);

    let dir = TempDir::new().unwrap();
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let mut job = snapshot(legacy.doc_id, 1, "draft");
    job.meta.encoding = EncodingSpec::Utf8Bom;
    job.meta.line_ending = LineEnding::Crlf;
    job.meta.saved_encoding = Some(EncodingSpec::Utf8Bom);
    job.meta.saved_line_ending = Some(LineEnding::Crlf);
    job.meta.language_override = Some("rust".into());
    admit(|| service.try_commit(job.clone()));
    receive(admit(|| service.flush()));
    receive(service.record_save_intent(legacy.clone()).unwrap());
    let published = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: legacy.doc_id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(published.meta.saved_encoding, None);
    assert_eq!(published.meta.saved_line_ending, None);
    job.base_rev = Revision(1);
    job.new_rev = Revision(2);
    admit(|| service.try_commit(job.clone()));
    receive(admit(|| service.flush()));
    let draft = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: legacy.doc_id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(draft.meta.saved_encoding, None);
    assert_eq!(draft.meta.saved_line_ending, None);
    assert_eq!(draft.meta.encoding, EncodingSpec::Utf8Bom);
    assert_eq!(draft.meta.line_ending, LineEnding::Crlf);
    assert_eq!(draft.meta.language_override.as_deref(), Some("rust"));
    receive(admit(|| service.shutdown()));
}

#[cfg(target_os = "macos")]
#[test]
fn real_save_intent_captures_request_format_without_reverting_newer_metadata() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("saved.txt");
    let service = RecoveryHandle::start(dir.path().join("db")).unwrap();
    let mut request = save_request(&path, "line\n");
    request.encoding = EncodingSpec::Utf8Bom;
    request.line_ending = LineEnding::Crlf;
    let mut newer = snapshot(request.doc_id, request.captured_rev.0 + 1, "line\nlater");
    newer.meta.language_override = Some("python".into());
    admit(|| service.try_commit(newer.clone()));
    receive(admit(|| service.flush()));
    assert!(matches!(
        save_document(&request, Some(&service), &FaultInjector::default()),
        SaveOutcome::Saved { .. }
    ));
    assert_eq!(fs::read(&path).unwrap(), b"\xef\xbb\xbfline\r\n");
    let intents = receive(service.request_save_intents().unwrap());
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].captured_encoding, Some(EncodingSpec::Utf8Bom));
    assert_eq!(intents[0].captured_line_ending, Some(LineEnding::Crlf));
    assert!(matches!(intents[0].stage, SaveStage::Published));
    let draft = receive(
        service
            .request_hydrate(
                DocumentKey {
                    doc_id: request.doc_id,
                    lifecycle_gen: LifecycleGen(1),
                },
                CancelToken::new(),
            )
            .unwrap(),
    );
    assert_eq!(draft.meta.saved_encoding, Some(EncodingSpec::Utf8Bom));
    assert_eq!(draft.meta.saved_line_ending, Some(LineEnding::Crlf));
    assert_eq!(draft.meta.encoding, EncodingSpec::Utf8);
    assert_eq!(draft.meta.line_ending, LineEnding::Lf);
    assert_eq!(draft.meta.language_override.as_deref(), Some("python"));
    assert_eq!(draft.rope, "line\nlater");
    let OpenOutcome::Editable {
        encoding,
        line_ending,
        ..
    } = admit(|| {
        open_document(
            DocumentId::new(),
            &path,
            &PagingService::default(),
            OpenOptions::default(),
            &CancelToken::new(),
        )
    })
    else {
        panic!("saved supported file must open editable");
    };
    assert_eq!(encoding, EncodingSpec::Utf8Bom);
    assert_eq!(line_ending, LineEnding::Crlf);
    receive(admit(|| service.shutdown()));
}

#[path = "support/session_groups.rs"]
mod session_groups;
