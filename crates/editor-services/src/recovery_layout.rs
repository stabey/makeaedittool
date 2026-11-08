//! Metadata-only session storage, used exclusively by the recovery worker.
use super::*;
use crate::RecoveryWarning;
use rusqlite::OpenFlags;
use std::collections::{HashMap, HashSet};

const SCHEMA: &str = "
CREATE TABLE layout_view_sets(id INTEGER PRIMARY KEY AUTOINCREMENT);
CREATE TABLE layout_states(id INTEGER PRIMARY KEY AUTOINCREMENT, format_version INTEGER NOT NULL, tree BLOB NOT NULL, view_set INTEGER NOT NULL REFERENCES layout_view_sets(id));
CREATE TABLE layout_views(view_set INTEGER NOT NULL REFERENCES layout_view_sets(id), view_id TEXT NOT NULL, doc TEXT NOT NULL REFERENCES documents(id), gen INTEGER NOT NULL, group_id TEXT NOT NULL, view_state TEXT NOT NULL, PRIMARY KEY(view_set,view_id), UNIQUE(view_set,doc));
CREATE INDEX layout_views_doc ON layout_views(doc);
CREATE TABLE layout_heads(slot TEXT PRIMARY KEY, layout_id INTEGER NOT NULL REFERENCES layout_states(id));
CREATE TABLE preserved_layouts(layout_id INTEGER PRIMARY KEY REFERENCES layout_states(id), reason TEXT NOT NULL);
CREATE TABLE closed_views(close_id INTEGER PRIMARY KEY AUTOINCREMENT, view_id TEXT NOT NULL, doc TEXT UNIQUE NOT NULL REFERENCES documents(id), gen INTEGER NOT NULL, pane_id TEXT NOT NULL, group_id TEXT NOT NULL, position INTEGER NOT NULL, view_state TEXT NOT NULL, closed_order INTEGER NOT NULL);
";
const DOCUMENT_SCHEMA: &str = "
CREATE TABLE documents(id TEXT PRIMARY KEY, gen INTEGER NOT NULL, state INTEGER NOT NULL, meta TEXT NOT NULL, saved_rev INTEGER NOT NULL DEFAULT -1, saved_meta TEXT);
CREATE TABLE recovery_heads(doc TEXT PRIMARY KEY REFERENCES documents(id), checkpoint_rev INTEGER NOT NULL, durable_rev INTEGER NOT NULL, byte_len INTEGER NOT NULL, checkpoint_hash BLOB NOT NULL, delta_bytes INTEGER NOT NULL DEFAULT 0, delta_rows INTEGER NOT NULL DEFAULT 0);
CREATE TABLE checkpoint_chunks(doc TEXT NOT NULL REFERENCES documents(id), rev INTEGER NOT NULL, ordinal INTEGER NOT NULL, content BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(doc,rev,ordinal));
CREATE TABLE edit_transactions(doc TEXT NOT NULL REFERENCES documents(id), base_rev INTEGER NOT NULL, new_rev INTEGER NOT NULL, deltas BLOB NOT NULL, checksum BLOB NOT NULL, PRIMARY KEY(doc,new_rev));
CREATE TABLE save_intents(doc TEXT PRIMARY KEY, value TEXT NOT NULL);
";
fn error(kind: ErrorKind, text: impl Into<String>) -> ServiceError {
    ServiceError::new(kind, text)
}

fn sql_unsigned(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(source),
        )
    })
}

fn sql_optional_unsigned(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(column)?;
    value.map(u64::try_from).transpose().map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(source),
        )
    })
}
fn schema_version(conn: &Connection) -> Result<(i64, i64), ServiceError> {
    Ok((
        conn.pragma_query_value(None, "user_version", |r| r.get(0))?,
        conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?,
    ))
}
fn preflight(conn: &Connection) -> Result<(i64, i64), ServiceError> {
    let state = schema_version(conn)?;
    if state.0 > 2 || state.0 < 0 || (state.0 == 0 && state.1 != 0) {
        return Err(error(
            ErrorKind::Unsupported,
            "unknown recovery schema; database and WAL left untouched",
        ));
    }
    if state.0 == 2 {
        // At most the two restorable heads and 1025 bounded records per view set.
        // Corrupt supported-format bodies are handled by manifest preservation;
        // recognized future fields must refuse the read-only preflight instead.
        let mut layouts = conn.prepare("SELECT s.format_version,CASE WHEN LENGTH(CAST(s.tree AS BLOB))<=524288 THEN CAST(s.tree AS BLOB) END,s.view_set FROM layout_states s WHERE s.id IN (SELECT layout_id FROM layout_heads WHERE slot IN ('current','last_valid'))")?;
        let mut views = conn.prepare("SELECT CASE WHEN typeof(view_state)='text' AND LENGTH(CAST(view_state AS BLOB))<=65536 THEN CAST(view_state AS BLOB) END FROM layout_views WHERE view_set=?1 LIMIT ?2")?;
        let mut rows = layouts.query([])?;
        let mut inspected_view_set = None;
        while let Some(row) = rows.next()? {
            let version: i64 = row.get(0)?;
            if !(1..=i64::from(LAYOUT_FORMAT_VERSION)).contains(&version) {
                return Err(error(
                    ErrorKind::Unsupported,
                    "unsupported layout format; preserved without opening a writer",
                ));
            }
            if let Some(tree) = row.get::<_, Option<Vec<u8>>>(1)? {
                if let Err(e) = reject_future_tree(&tree, version as u32)
                    && e.kind == ErrorKind::Unsupported
                {
                    return Err(e);
                }
            }
            let set: i64 = row.get(2)?;
            if inspected_view_set == Some(set) {
                continue;
            }
            let mut view_rows = views.query(params![set, MAX_SESSION_VIEWS as i64 + 1])?;
            while let Some(view_row) = view_rows.next()? {
                if let Some(bytes) = view_row.get::<_, Option<Vec<u8>>>(0)?
                    && let Err(e) = reject_future_view_state(&bytes)
                    && e.kind == ErrorKind::Unsupported
                {
                    return Err(e);
                }
            }
            inspected_view_set = Some(set);
        }
    }
    Ok(state)
}
fn reject_unknown_layout_fields(
    value: &serde_json::Value,
    known: &[&str],
) -> Result<(), ServiceError> {
    if value
        .as_object()
        .is_some_and(|object| object.keys().any(|key| !known.contains(&key.as_str())))
    {
        return Err(error(
            ErrorKind::Unsupported,
            "unsupported layout fields; preserved without opening a writer",
        ));
    }
    Ok(())
}
fn reject_future_tree(bytes: &[u8], sql_version: u32) -> Result<serde_json::Value, ServiceError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let mut corrupt = None;
    let version = match value.get("format_version") {
        Some(version) if version.is_number() => version
            .as_u64()
            .ok_or_else(|| error(ErrorKind::Unsupported, "invalid layout version"))?,
        _ => {
            corrupt = Some(error(
                ErrorKind::Corrupt,
                "missing or invalid layout version",
            ));
            u64::from(sql_version)
        }
    };
    if !(1..=u64::from(LAYOUT_FORMAT_VERSION)).contains(&version) {
        return Err(error(
            ErrorKind::Unsupported,
            "unsupported layout version; preserved without opening a writer",
        ));
    }
    let keys: &[&str] = if version == 1 {
        &["format_version", "groups", "root"]
    } else {
        &["format_version", "groups", "active_pane", "root"]
    };
    reject_unknown_layout_fields(&value, keys)?;
    let mut nodes = 0;
    let mut panes = 0;
    if let Some(root) = value.get("root") {
        inspect_tree_node(root, version, 1, &mut nodes, &mut panes, &mut corrupt)?;
    } else {
        corrupt.get_or_insert_with(|| error(ErrorKind::Corrupt, "missing root"));
    }
    if let Some(groups) = value.get("groups").and_then(|v| v.as_array()) {
        if groups.len() > MAX_CUSTOM_GROUPS + 1 {
            corrupt.get_or_insert_with(|| error(ErrorKind::Corrupt, "group bound exceeded"));
        }
        for group in groups.iter().take(MAX_CUSTOM_GROUPS + 1) {
            reject_unknown_layout_fields(group, &["id", "name", "color", "collapsed"])?;
        }
    }
    if version != u64::from(sql_version) {
        corrupt.get_or_insert_with(|| {
            error(ErrorKind::Corrupt, "SQL and JSON layout versions disagree")
        });
    }
    if let Some(corrupt) = corrupt {
        return Err(corrupt);
    }
    Ok(value)
}
fn inspect_tree_node(
    value: &serde_json::Value,
    version: u64,
    depth: usize,
    nodes: &mut usize,
    panes: &mut usize,
    corrupt: &mut Option<ServiceError>,
) -> Result<(), ServiceError> {
    if depth > MAX_LAYOUT_NODES || *nodes >= MAX_LAYOUT_NODES {
        corrupt.get_or_insert_with(|| error(ErrorKind::Corrupt, "layout traversal bound exceeded"));
        return Ok(());
    }
    *nodes += 1;
    reject_unknown_layout_fields(
        value,
        if version == 1 {
            &["Pane"]
        } else {
            &["Pane", "Split"]
        },
    )?;
    if let Some(node) = value.get("Pane") {
        *panes += 1;
        if *panes > MAX_PANES {
            corrupt.get_or_insert_with(|| error(ErrorKind::Corrupt, "pane bound exceeded"));
        }
        reject_unknown_layout_fields(node, &["node_id", "pane"])?;
        if let Some(pane) = node.get("pane") {
            reject_unknown_layout_fields(pane, &["id", "tabs", "active_view"])?;
        }
    }
    if let Some(node) = value.get("Split") {
        reject_unknown_layout_fields(node, &["node_id", "axis", "ratio_bps", "first", "second"])?;
        if let Some(axis) = node.get("axis").and_then(|v| v.as_str()) {
            if axis != "Horizontal" && axis != "Vertical" {
                return Err(error(ErrorKind::Unsupported, "unsupported split axis"));
            }
        }
        for key in ["first", "second"] {
            if let Some(child) = node.get(key) {
                inspect_tree_node(child, version, depth + 1, nodes, panes, corrupt)?;
            } else {
                corrupt.get_or_insert_with(|| error(ErrorKind::Corrupt, "missing split child"));
            }
        }
    }
    Ok(())
}
fn reject_future_view_state(bytes: &[u8]) -> Result<(), ServiceError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    reject_unknown_layout_fields(&value, &["cursor_byte", "scroll_byte"])
}
pub(super) fn open_database(
    path: &Path,
    faults: &FaultInjector,
) -> Result<Connection, ServiceError> {
    let exists = match std::fs::metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    let observed = if exists {
        let reader = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        reader.busy_timeout(Duration::from_secs(2))?;
        Some(preflight(&reader)?)
    } else {
        None
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | if exists {
            OpenFlags::empty()
        } else {
            OpenFlags::SQLITE_OPEN_CREATE
        };
    let mut conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    {
        // No writable pragma, DDL, or checkpoint precedes this recheck.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = preflight(&tx)?;
        if observed.is_some_and(|v| v != current) {
            return Err(error(
                ErrorKind::Stale,
                "schema changed during recovery preflight",
            ));
        }
        match current.0 {
            0 => {
                tx.execute_batch(DOCUMENT_SCHEMA)?;
                tx.execute_batch(SCHEMA)?;
                install_layout(&tx, &SessionLayout::default(), None)?;
            }
            1 => {
                tx.execute_batch("ALTER TABLE windows_views RENAME TO legacy_v1_windows_views; ALTER TABLE session RENAME TO legacy_v1_session;")?;
                tx.execute_batch(SCHEMA)?;
                let layout = migrate_legacy(&tx)?;
                install_layout(&tx, &layout, None)?;
            }
            2 => {}
            _ => return Err(error(ErrorKind::Unsupported, "unsupported recovery schema")),
        }
        if current.0 < 2 {
            faults.check(FaultPoint::RecoveryBeforeCommit)?;
            tx.pragma_update(None, "user_version", 2)?;
        }
        tx.commit()?;
    }
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=512; PRAGMA journal_size_limit=16777216;")?;
    Ok(conn)
}
fn migrate_legacy(conn: &Connection) -> Result<SessionLayout, ServiceError> {
    let mut layout = SessionLayout::default();
    let active: Option<Vec<u8>> = conn.query_row("SELECT CASE WHEN typeof(value)='text' AND LENGTH(CAST(value AS BLOB))<=64 THEN CAST(value AS BLOB) END FROM legacy_v1_session WHERE key='active'", [], |row| row.get(0)).optional()?.flatten();
    let active = active.and_then(|bytes| String::from_utf8(bytes).ok());
    let mut statement = conn.prepare("SELECT CASE WHEN typeof(v.doc)='text' AND LENGTH(CAST(v.doc AS BLOB))=32 THEN CAST(v.doc AS BLOB) END,CASE WHEN typeof(v.view)='text' AND LENGTH(CAST(v.view AS BLOB))<=65536 THEN CAST(v.view AS BLOB) END FROM legacy_v1_windows_views v JOIN documents d ON d.id=v.doc WHERE d.state=0 ORDER BY v.position,v.rowid")?;
    let mut rows = statement.query([])?;
    // Stream every legacy row, including overflow. Archives retain raw bytes in SQLite.
    while let Some(row) = rows.next()? {
        if layout.views.len() == MAX_SESSION_VIEWS {
            continue;
        }
        let Some(id) = row
            .get::<_, Option<Vec<u8>>>(0)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        let Ok(stub) = document_stub(conn, &id) else {
            continue;
        };
        let view = row
            .get::<_, Option<Vec<u8>>>(1)?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let view_id = ViewId::new();
        layout.insert_view(
            layout.active_pane_id(),
            LayoutView {
                id: view_id,
                document: stub.key(),
                group_id: GroupId::UNGROUPED,
                view,
            },
            None,
            active.as_deref() == Some(&id),
        )?;
    }
    let mut statement = conn.prepare("SELECT CASE WHEN typeof(d.id)='text' AND LENGTH(CAST(d.id AS BLOB))=32 THEN CAST(d.id AS BLOB) END FROM documents d JOIN recovery_heads h ON h.doc=d.id WHERE d.state=0 ORDER BY d.rowid")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if layout.views.len() == MAX_SESSION_VIEWS {
            continue;
        }
        let Some(id) = row
            .get::<_, Option<Vec<u8>>>(0)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        let Ok(stub) = document_stub(conn, &id) else {
            continue;
        };
        if layout
            .views
            .values()
            .any(|v| v.document.doc_id == stub.doc_id)
        {
            continue;
        }
        layout.insert_view(
            layout.active_pane_id(),
            LayoutView {
                id: ViewId::new(),
                document: stub.key(),
                group_id: GroupId::UNGROUPED,
                view: ViewState::default(),
            },
            None,
            active.as_deref() == Some(&id),
        )?;
    }
    Ok(layout)
}
fn head(conn: &Connection, slot: &str) -> Result<Option<LayoutRevision>, ServiceError> {
    conn.query_row(
        "SELECT layout_id FROM layout_heads WHERE slot=?1",
        [slot],
        |r| sql_unsigned(r, 0),
    )
    .optional()
    .map(|v| v.map(LayoutRevision))
    .map_err(Into::into)
}
fn expect_head(conn: &Connection, expected: LayoutRevision) -> Result<(), ServiceError> {
    if head(conn, "current")? != Some(expected) {
        return Err(error(ErrorKind::Stale, "durable layout revision changed"));
    }
    Ok(())
}
fn pin(conn: &Connection, revision: LayoutRevision, reason: &str) -> Result<(), ServiceError> {
    conn.execute("INSERT OR IGNORE INTO preserved_layouts(layout_id,reason) SELECT id,?2 FROM layout_states WHERE id=?1", params![sql_rev(revision.0)?, reason])?;
    Ok(())
}
fn install_layout(
    conn: &Connection,
    layout: &SessionLayout,
    shared_set: Option<i64>,
) -> Result<LayoutRevision, ServiceError> {
    layout.validate()?;
    let previous = head(conn, "current")?;
    let previous_is_valid = if let Some(previous) = previous {
        match load_layout(conn, previous) {
            Ok(_) => true,
            Err(e) if e.kind == ErrorKind::Unsupported => return Err(e),
            Err(_) => {
                // Every installation owns preservation, even if no fresh manifest
                // has observed an externally damaged current snapshot.
                pin(conn, previous, "invalid_current")?;
                false
            }
        }
    } else {
        false
    };
    if let Some(last_valid) = head(conn, "last_valid")?
        && Some(last_valid) != previous
    {
        match load_layout(conn, last_valid) {
            Ok(_) => {}
            Err(e) if e.kind == ErrorKind::Unsupported => return Err(e),
            Err(_) => pin(conn, last_valid, "invalid_last_valid")?,
        }
    }
    let set = if let Some(set) = shared_set {
        set
    } else {
        conn.execute("INSERT INTO layout_view_sets DEFAULT VALUES", [])?;
        let set = conn.last_insert_rowid();
        let mut insert = conn.prepare("INSERT INTO layout_views(view_set,view_id,doc,gen,group_id,view_state) VALUES(?1,?2,?3,?4,?5,?6)")?;
        for view in layout.views.values() {
            insert.execute(params![
                set,
                view.id.key(),
                view.document.doc_id.key(),
                sql_rev(view.document.lifecycle_gen.0)?,
                view.group_id.key(),
                serde_json::to_string(&view.view)?
            ])?;
        }
        set
    };
    conn.execute(
        "INSERT INTO layout_states(format_version,tree,view_set) VALUES(?1,?2,?3)",
        params![
            layout.tree.format_version,
            serde_json::to_vec(&layout.tree)?,
            set
        ],
    )?;
    let revision = LayoutRevision(conn.last_insert_rowid() as u64);
    if previous_is_valid {
        conn.execute("INSERT INTO layout_heads(slot,layout_id) SELECT 'last_valid',layout_id FROM layout_heads WHERE slot='current' ON CONFLICT(slot) DO UPDATE SET layout_id=excluded.layout_id", [])?;
    }
    conn.execute("INSERT INTO layout_heads(slot,layout_id) VALUES('current',?1) ON CONFLICT(slot) DO UPDATE SET layout_id=excluded.layout_id", [sql_rev(revision.0)?])?;
    conn.execute("DELETE FROM layout_states WHERE id NOT IN (SELECT layout_id FROM layout_heads) AND id NOT IN (SELECT layout_id FROM preserved_layouts)", [])?;
    conn.execute(
        "DELETE FROM layout_views WHERE view_set NOT IN (SELECT view_set FROM layout_states)",
        [],
    )?;
    conn.execute(
        "DELETE FROM layout_view_sets WHERE id NOT IN (SELECT view_set FROM layout_states)",
        [],
    )?;
    Ok(revision)
}
fn load_layout(
    conn: &Connection,
    revision: LayoutRevision,
) -> Result<(SessionLayout, i64), ServiceError> {
    let (version, len, set): (u32, i64, i64) = conn.query_row(
        "SELECT format_version,LENGTH(CAST(tree AS BLOB)),view_set FROM layout_states WHERE id=?1",
        [sql_rev(revision.0)?],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if !(1..=LAYOUT_FORMAT_VERSION).contains(&version) {
        return Err(error(ErrorKind::Unsupported, "unsupported layout format"));
    }
    if len < 0 || len as usize > MAX_LAYOUT_TREE_BYTES {
        return Err(error(
            ErrorKind::Corrupt,
            format!("layout tree has {len} bytes, exceeds decode bound"),
        ));
    }
    let bytes: Vec<u8> = conn.query_row(
        "SELECT CAST(tree AS BLOB) FROM layout_states WHERE id=?1",
        [sql_rev(revision.0)?],
        |r| r.get(0),
    )?;
    let mut value = reject_future_tree(&bytes, version)?;
    if version == 1 {
        let pane = value
            .pointer("/root/Pane/pane/id")
            .cloned()
            .ok_or_else(|| error(ErrorKind::Corrupt, "missing legacy pane"))?;
        value["active_pane"] = pane;
        value["format_version"] = LAYOUT_FORMAT_VERSION.into();
    }
    let tree: LayoutTree = serde_json::from_value(value)?;
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM layout_views WHERE view_set=?1",
        [set],
        |r| r.get(0),
    )?;
    if count > MAX_SESSION_VIEWS as i64 {
        return Err(error(
            ErrorKind::Corrupt,
            "layout reference set exceeds capacity",
        ));
    }
    let mut views = HashMap::with_capacity(count as usize);
    let mut stmt = conn.prepare("SELECT CASE WHEN typeof(view_id)='text' AND LENGTH(CAST(view_id AS BLOB))=32 THEN view_id END,CASE WHEN typeof(doc)='text' AND LENGTH(CAST(doc AS BLOB))=32 THEN doc END,gen,CASE WHEN typeof(group_id)='text' AND LENGTH(CAST(group_id AS BLOB))=32 THEN group_id END,CASE WHEN typeof(view_state)='text' AND LENGTH(CAST(view_state AS BLOB))<=65536 THEN CAST(view_state AS BLOB) END FROM layout_views WHERE view_set=?1")?;
    let mut rows = stmt.query([set])?;
    while let Some(row) = rows.next()? {
        let id = ViewId::from_key(&row.get::<_, String>(0)?)?;
        let generation = sql_unsigned(row, 2)?;
        let json: Option<Vec<u8>> = row.get(4)?;
        let json = json.ok_or_else(|| {
            error(
                ErrorKind::Corrupt,
                "view metadata is not bounded UTF-8 JSON text",
            )
        })?;
        reject_future_view_state(&json)?;
        let view = serde_json::from_slice(&json)?;
        views.insert(
            id,
            LayoutView {
                id,
                document: DocumentKey {
                    doc_id: parse_id(&row.get::<_, String>(1)?)?,
                    lifecycle_gen: LifecycleGen(generation),
                },
                group_id: GroupId::from_key(&row.get::<_, String>(3)?)?,
                view,
            },
        );
    }
    let layout = SessionLayout { tree, views };
    layout.validate()?;
    Ok((layout, set))
}
pub(super) fn document_stub(
    conn: &Connection,
    id: &str,
) -> Result<SessionDocumentStub, ServiceError> {
    let doc_id = parse_id(id)?;
    let (generation,state,length,json,revision,checkpoint,meta_type):(Option<u64>,Option<i64>,i64,Option<Vec<u8>>,Option<u64>,bool,String)=conn.query_row("SELECT CASE WHEN typeof(d.gen)='integer' AND d.gen>=0 THEN d.gen END,CASE WHEN typeof(d.state)='integer' AND d.state IN(0,1,2) THEN d.state END,COALESCE(LENGTH(CAST(d.meta AS BLOB)),0),CASE WHEN typeof(d.meta)='text' AND LENGTH(CAST(d.meta AS BLOB))<=65536 THEN CAST(d.meta AS BLOB) END,CASE WHEN h.doc IS NULL THEN 0 WHEN typeof(h.durable_rev)='integer' AND h.durable_rev>=0 THEN h.durable_rev END,h.doc IS NOT NULL,typeof(d.meta) FROM documents d LEFT JOIN recovery_heads h ON h.doc=d.id WHERE d.id=?1",[id],|row|Ok((sql_optional_unsigned(row,0)?,row.get(1)?,row.get(2)?,row.get(3)?,sql_optional_unsigned(row,4)?,row.get(5)?,row.get(6)?)))?;
    let generation =
        generation.ok_or_else(|| error(ErrorKind::Corrupt, "invalid document lifecycle"))?;
    let state = state.ok_or_else(|| {
        error(
            ErrorKind::Corrupt,
            "unknown document state; record is inaccessible and preserved",
        )
    })?;
    if state == 2 {
        return Err(error(ErrorKind::Stale, "document was discarded"));
    }
    let revision =
        revision.ok_or_else(|| error(ErrorKind::Corrupt, "invalid checkpoint durable revision"))?;
    let json = json.ok_or_else(|| {
        error(
            ErrorKind::Corrupt,
            format!("invalid document metadata ({meta_type}, {length} bytes); expected UTF-8 JSON text up to 65536 bytes"),
        )
    })?;
    Ok(SessionDocumentStub {
        doc_id,
        lifecycle_gen: LifecycleGen(generation),
        durable_rev: Revision(revision),
        meta: serde_json::from_slice(&json)?,
        has_checkpoint: checkpoint,
    })
}
fn validate_references(conn: &Connection, layout: &SessionLayout) -> Result<(), ServiceError> {
    layout.validate()?;
    for view in layout.views.values() {
        let row: Option<(u64, i64)> = conn
            .query_row(
                "SELECT gen,state FROM documents WHERE id=?1",
                [view.document.doc_id.key()],
                |r| Ok((sql_unsigned(r, 0)?, r.get(1)?)),
            )
            .optional()?;
        if row != Some((view.document.lifecycle_gen.0, 0)) {
            return Err(error(
                ErrorKind::Stale,
                "layout references a missing, closed or obsolete document",
            ));
        }
    }
    Ok(())
}
pub(super) fn manifest(conn: &mut Connection) -> Result<SessionManifest, ServiceError> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let manifest = read_manifest(&transaction, true)?;
    transaction.commit()?;
    Ok(manifest)
}
pub(super) fn read_only_manifest(conn: &Connection) -> Result<SessionManifest, ServiceError> {
    read_manifest(conn, false)
}
fn read_manifest(conn: &Connection, preserve: bool) -> Result<SessionManifest, ServiceError> {
    let current = head(conn, "current")?
        .ok_or_else(|| error(ErrorKind::Corrupt, "missing current layout"))?;
    let mut warnings = Vec::new();
    let mut source = current;
    let mut repaired_or_fallback = false;
    let mut layout = match load_layout(conn, current) {
        Ok((layout, _)) => layout,
        Err(e) => {
            repaired_or_fallback = true;
            if e.kind == ErrorKind::Unsupported {
                return Err(e);
            }
            if preserve {
                pin(conn, current, &format!("invalid_current: {e}"))?;
            }
            warnings.push(RecoveryWarning {
                doc_id: None,
                message: format!("Current layout preserved: {e}"),
            });
            match head(conn, "last_valid")? {
                Some(previous) => match load_layout(conn, previous) {
                    Ok((layout, _)) => {
                        source = previous;
                        layout
                    }
                    Err(e) if e.kind == ErrorKind::Unsupported => return Err(e),
                    Err(e) => {
                        if preserve {
                            pin(conn, previous, &format!("invalid_last_valid: {e}"))?;
                        }
                        fallback_layout(conn)?
                    }
                },
                None => fallback_layout(conn)?,
            }
        }
    };
    if layout.views.is_empty() {
        populate_empty_layout_fallback(conn, &mut layout)?;
        repaired_or_fallback |= !layout.views.is_empty();
    }
    let mut documents = Vec::with_capacity(layout.views.len());
    let mut removed = Vec::new();
    for id in layout.view_ids() {
        let view = &layout.views[&id];
        let key = view.document;
        let state:Option<(Option<u64>,Option<i64>)>=conn.query_row("SELECT CASE WHEN typeof(gen)='integer' AND gen>=0 THEN gen END,CASE WHEN typeof(state)='integer' AND state IN(0,1,2) THEN state END FROM documents WHERE id=?1",[key.doc_id.key()],|row|Ok((sql_optional_unsigned(row,0)?,row.get(1)?))).optional()?;
        if state != Some((Some(key.lifecycle_gen.0), Some(0))) {
            removed.push(ViewTarget {
                view_id: id,
                document: key,
            });
            continue;
        }
        match document_stub(conn, &key.doc_id.key()) {
            Ok(stub) => documents.push(stub),
            Err(e) => {
                removed.push(ViewTarget {
                    view_id: id,
                    document: key,
                });
                warnings.push(RecoveryWarning {
                    doc_id: Some(key.doc_id),
                    message: format!("Unopened metadata record preserved in recovery browser: {e}"),
                });
            }
        }
    }
    if !removed.is_empty() {
        repaired_or_fallback = true;
        if preserve {
            pin(conn, source, "isolated_unreadable_or_stale_references")?;
        }
        layout.remove_views(&removed)?;
    }
    layout.validate()?;
    let recovery = catalog_summary(conn, &layout)?;
    Ok(SessionManifest {
        layout,
        layout_revision: current,
        source_layout_revision: Some(source),
        repaired_or_fallback,
        documents,
        recovery,
        warnings,
    })
}
fn fallback_layout(conn: &Connection) -> Result<SessionLayout, ServiceError> {
    let mut layout = SessionLayout::default();
    populate_empty_layout_fallback(conn, &mut layout)?;
    Ok(layout)
}
fn populate_empty_layout_fallback(
    conn: &Connection,
    layout: &mut SessionLayout,
) -> Result<(), ServiceError> {
    let eligible = eligibility(conn)?;
    let mut stmt = conn.prepare(&format!("SELECT CASE WHEN typeof(d.id)='text' AND LENGTH(CAST(d.id AS BLOB))=32 THEN CAST(d.id AS BLOB) END FROM documents d LEFT JOIN recovery_heads h ON h.doc=d.id WHERE d.state=0 AND ({eligible}) ORDER BY h.doc IS NULL,d.rowid"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if layout.views.len() == MAX_SESSION_VIEWS {
            break;
        }
        let Some(id) = row
            .get::<_, Option<Vec<u8>>>(0)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
        else {
            continue;
        };
        let Ok(stub) = document_stub(conn, &id) else {
            continue;
        };
        layout.insert_view(
            layout.active_pane_id(),
            LayoutView {
                id: ViewId::new(),
                document: stub.key(),
                group_id: GroupId::UNGROUPED,
                view: ViewState::default(),
            },
            None,
            false,
        )?;
    }
    Ok(())
}
const CATALOG_LIVE_DOCUMENT: &str = "(typeof(d.state)<>'integer' OR d.state<>2)";
const LEGACY_ORPHAN_PREDICATE: &str =
    "NOT EXISTS(SELECT 1 FROM documents d WHERE typeof(v.doc)='text' AND d.id=v.doc)";
const LEGACY_ORPHAN_CHECKPOINT: &str = "typeof(v.doc)='text' AND LENGTH(CAST(v.doc AS BLOB))<=64 AND EXISTS(SELECT 1 FROM recovery_heads h WHERE h.doc=v.doc)";
fn legacy_archive_exists(conn: &Connection) -> Result<bool, ServiceError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='legacy_v1_windows_views')",[],|row|row.get(0))?)
}
fn eligibility(conn: &Connection) -> Result<String, ServiceError> {
    let legacy = legacy_archive_exists(conn)?;
    Ok(format!(
        "h.doc IS NOT NULL OR EXISTS(SELECT 1 FROM layout_views v WHERE v.doc=d.id) OR EXISTS(SELECT 1 FROM closed_views c WHERE c.doc=d.id){}",
        if legacy {
            " OR EXISTS(SELECT 1 FROM legacy_v1_windows_views v WHERE v.doc=d.id)"
        } else {
            ""
        }
    ))
}
fn catalog_summary(
    conn: &Connection,
    layout: &SessionLayout,
) -> Result<RecoveryCatalogSummary, ServiceError> {
    let eligible = eligibility(conn)?;
    let (closed,checkpoint,other):(u64,u64,u64)=conn.query_row(&format!("SELECT COALESCE(sum(typeof(d.state)='integer' AND d.state=1),0),COALESCE(sum(NOT(typeof(d.state)='integer' AND d.state=1) AND h.doc IS NOT NULL),0),COALESCE(sum(NOT(typeof(d.state)='integer' AND d.state=1) AND h.doc IS NULL),0) FROM documents d LEFT JOIN recovery_heads h ON h.doc=d.id WHERE {CATALOG_LIVE_DOCUMENT} AND ({eligible})"),[],|row|Ok((sql_unsigned(row,0)?,sql_unsigned(row,1)?,sql_unsigned(row,2)?)))?;
    let mut admitted = 0u16;
    let mut admitted_checkpoint = 0u64;
    for view in layout.views.values() {
        let row:Option<(Option<u64>,bool,bool)>=conn.query_row("SELECT CASE WHEN typeof(d.gen)='integer' AND d.gen>=0 THEN d.gen END,CASE WHEN typeof(d.state)='integer' AND d.state=0 THEN 1 ELSE 0 END,EXISTS(SELECT 1 FROM recovery_heads h WHERE h.doc=d.id) FROM documents d WHERE d.id=?1",[view.document.doc_id.key()],|row|Ok((sql_optional_unsigned(row,0)?,row.get(1)?,row.get(2)?))).optional()?;
        if let Some((Some(generation), true, checkpoint)) = row {
            if generation == view.document.lifecycle_gen.0 {
                admitted += 1;
                admitted_checkpoint += u64::from(checkpoint);
            }
        }
    }
    let (orphan_total, orphan_checkpoints): (u64, u64) = if legacy_archive_exists(conn)? {
        conn.query_row(&format!("SELECT count(*),COALESCE(sum({LEGACY_ORPHAN_CHECKPOINT}),0) FROM legacy_v1_windows_views v WHERE {LEGACY_ORPHAN_PREDICATE}"),[],|row|Ok((sql_unsigned(row,0)?,sql_unsigned(row,1)?)))?
    } else {
        (0, 0)
    };
    let admitted_other = u64::from(admitted) - admitted_checkpoint;
    let unplaced_checkpoints = checkpoint - admitted_checkpoint + orphan_checkpoints;
    let unplaced_other = other - admitted_other + orphan_total - orphan_checkpoints;
    Ok(RecoveryCatalogSummary {
        total_eligible: u64::from(admitted) + closed + unplaced_checkpoints + unplaced_other,
        admitted,
        closed,
        unplaced_checkpoints,
        unplaced_other,
    })
}
pub(super) fn validate_page_limit(limit: u16) -> Result<(), ServiceError> {
    if !(1..=64).contains(&limit) {
        Err(error(ErrorKind::InvalidInput, "page size must be 1–64"))
    } else {
        Ok(())
    }
}
pub(super) fn recovery_page(
    conn: &Connection,
    request: RecoveryPageRequest,
) -> Result<RecoveryPage, ServiceError> {
    if !conn.is_autocommit() {
        return recovery_page_snapshot(conn, request);
    }
    let transaction = conn.unchecked_transaction()?;
    let page = recovery_page_snapshot(&transaction, request)?;
    transaction.commit()?;
    Ok(page)
}
fn recovery_page_snapshot(
    conn: &Connection,
    request: RecoveryPageRequest,
) -> Result<RecoveryPage, ServiceError> {
    validate_page_limit(request.limit)?;
    let manifest = read_only_manifest(conn)?;
    let summary = manifest.recovery;
    let legacy = legacy_archive_exists(conn)?;
    let mut cursor = match request.cursor {
        Some(cursor) => cursor,
        None => RecoveryCursor {
            origin: RecoveryEntryOrigin::Document,
            after_rowid: None,
            document_upper_rowid: conn.query_row(
                "SELECT MAX(rowid) FROM documents",
                [],
                |row| row.get(0),
            )?,
            legacy_orphan_upper_rowid: if legacy {
                conn.query_row(
                    "SELECT MAX(rowid) FROM legacy_v1_windows_views",
                    [],
                    |row| row.get(0),
                )?
            } else {
                None
            },
        },
    };
    let mut admitted = if cursor.origin == RecoveryEntryOrigin::Document {
        manifest
            .documents
            .into_iter()
            .map(|stub| (stub.key(), stub))
            .collect::<HashMap<_, _>>()
    } else {
        HashMap::new()
    };
    let mut entries = Vec::with_capacity(request.limit as usize);
    let mut page_unreadable = 0;
    if cursor.origin == RecoveryEntryOrigin::Document {
        if append_document_catalog(
            conn,
            &mut cursor,
            request.limit,
            &mut admitted,
            &mut entries,
            &mut page_unreadable,
        )? {
            return Ok(RecoveryPage {
                entries,
                next: Some(cursor),
                summary,
                page_unreadable,
            });
        }
        cursor.origin = RecoveryEntryOrigin::LegacyOrphan;
        cursor.after_rowid = None;
    }
    let more = legacy
        && append_legacy_orphans(
            conn,
            &mut cursor,
            request.limit,
            &mut entries,
            &mut page_unreadable,
        )?;
    Ok(RecoveryPage {
        entries,
        next: more.then_some(cursor),
        summary,
        page_unreadable,
    })
}
fn append_document_catalog(
    conn: &Connection,
    cursor: &mut RecoveryCursor,
    limit: u16,
    admitted: &mut HashMap<DocumentKey, SessionDocumentStub>,
    entries: &mut Vec<RecoveryEntry>,
    unreadable: &mut u16,
) -> Result<bool, ServiceError> {
    let Some(upper) = cursor.document_upper_rowid else {
        return Ok(false);
    };
    if cursor.after_rowid.is_some_and(|after| after >= upper) {
        return Ok(false);
    }
    let range = if cursor.after_rowid.is_some() {
        "d.rowid>?1 AND d.rowid<=?2"
    } else {
        "d.rowid<=?2"
    };
    let eligible = eligibility(conn)?;
    let mut statement=conn.prepare(&format!("SELECT d.rowid,CASE WHEN typeof(d.id)='text' AND LENGTH(CAST(d.id AS BLOB))<=64 THEN CAST(d.id AS BLOB) END,CASE WHEN typeof(d.gen)='integer' AND d.gen>=0 THEN d.gen END,CASE WHEN typeof(d.state)='integer' AND d.state=1 THEN 1 ELSE 0 END,h.doc IS NOT NULL,COALESCE(LENGTH(CAST(d.id AS BLOB)),0),typeof(d.id) FROM documents d LEFT JOIN recovery_heads h ON h.doc=d.id WHERE {CATALOG_LIVE_DOCUMENT} AND {range} AND ({eligible}) ORDER BY d.rowid LIMIT ?3"))?;
    let mut rows = statement.query(params![
        cursor.after_rowid,
        upper,
        (usize::from(limit) - entries.len() + 1) as i64
    ])?;
    while let Some(row) = rows.next()? {
        if entries.len() == usize::from(limit) {
            return Ok(true);
        }
        let rowid = row.get(0)?;
        let bytes: Option<Vec<u8>> = row.get(1)?;
        let generation = sql_optional_unsigned(row, 2)?;
        let closed: bool = row.get(3)?;
        let has_checkpoint: bool = row.get(4)?;
        let id_bytes: i64 = row.get(5)?;
        let id_type: String = row.get(6)?;
        let id = bytes
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        let key = RecoveryEntryKey {
            origin: RecoveryEntryOrigin::Document,
            rowid,
            doc_id: id.and_then(|id| parse_id(id).ok()),
            lifecycle_gen: generation.map(LifecycleGen),
        };
        let admitted_stub =
            key.doc_id
                .zip(key.lifecycle_gen)
                .and_then(|(doc_id, lifecycle_gen)| {
                    admitted.remove(&DocumentKey {
                        doc_id,
                        lifecycle_gen,
                    })
                });
        let is_admitted = admitted_stub.is_some();
        let stub = match admitted_stub {
            Some(stub) => Ok(stub),
            None => match id {
                Some(id) => document_stub(conn, id),
                None => Err(error(
                    ErrorKind::Corrupt,
                    format!(
                        "invalid document identity ({id_type}, {id_bytes} bytes); record preserved"
                    ),
                )),
            },
        };
        if stub.is_err() {
            *unreadable += 1;
        }
        let last_placement = id.and_then(|id| closed_for_document(conn, id).ok().flatten());
        entries.push(RecoveryEntry {
            key,
            category: if closed {
                RecoveryEntryCategory::Closed
            } else if is_admitted {
                RecoveryEntryCategory::Admitted
            } else if has_checkpoint {
                RecoveryEntryCategory::UnplacedCheckpoint
            } else {
                RecoveryEntryCategory::UnplacedOther
            },
            has_checkpoint,
            stub,
            last_placement,
        });
        cursor.after_rowid = Some(rowid);
    }
    Ok(false)
}
fn append_legacy_orphans(
    conn: &Connection,
    cursor: &mut RecoveryCursor,
    limit: u16,
    entries: &mut Vec<RecoveryEntry>,
    unreadable: &mut u16,
) -> Result<bool, ServiceError> {
    let Some(upper) = cursor.legacy_orphan_upper_rowid else {
        return Ok(false);
    };
    if cursor.after_rowid.is_some_and(|after| after >= upper) {
        return Ok(false);
    }
    let range = if cursor.after_rowid.is_some() {
        "v.rowid>?1 AND v.rowid<=?2"
    } else {
        "v.rowid<=?2"
    };
    let mut statement=conn.prepare(&format!("SELECT v.rowid,CASE WHEN typeof(v.doc)='text' AND LENGTH(CAST(v.doc AS BLOB))<=64 THEN CAST(v.doc AS BLOB) END,({LEGACY_ORPHAN_CHECKPOINT}),typeof(v.doc),COALESCE(LENGTH(CAST(v.doc AS BLOB)),0),typeof(v.view),COALESCE(LENGTH(CAST(v.view AS BLOB)),0) FROM legacy_v1_windows_views v WHERE {LEGACY_ORPHAN_PREDICATE} AND {range} ORDER BY v.rowid LIMIT ?3"))?;
    let mut rows = statement.query(params![
        cursor.after_rowid,
        upper,
        (usize::from(limit) - entries.len() + 1) as i64
    ])?;
    while let Some(row) = rows.next()? {
        if entries.len() == usize::from(limit) {
            return Ok(true);
        }
        let rowid = row.get(0)?;
        let bytes: Option<Vec<u8>> = row.get(1)?;
        let has_checkpoint: bool = row.get(2)?;
        let doc_type: String = row.get(3)?;
        let doc_bytes: i64 = row.get(4)?;
        let view_type: String = row.get(5)?;
        let view_bytes: i64 = row.get(6)?;
        let doc_id = bytes
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(|id| parse_id(id).ok());
        let key = RecoveryEntryKey {
            origin: RecoveryEntryOrigin::LegacyOrphan,
            rowid,
            doc_id,
            lifecycle_gen: None,
        };
        let reason = format!(
            "Preserved legacy v1 placement has no matching document metadata (document key: {doc_type}, {doc_bytes} bytes; view metadata: {view_type}, {view_bytes} bytes). This orphan is inaccessible; no content is assumed recoverable."
        );
        entries.push(RecoveryEntry {
            key,
            category: if has_checkpoint {
                RecoveryEntryCategory::UnplacedCheckpoint
            } else {
                RecoveryEntryCategory::UnplacedOther
            },
            has_checkpoint,
            stub: Err(error(ErrorKind::Corrupt, reason)),
            last_placement: None,
        });
        *unreadable += 1;
        cursor.after_rowid = Some(rowid);
    }
    Ok(false)
}
fn read_closed(conn: &Connection, close_id: u64) -> Result<ClosedViewRecord, ServiceError> {
    let (view,id,generation,pane,group,position,json):(String,String,u64,String,String,u16,Option<Vec<u8>>)=conn.query_row("SELECT CASE WHEN typeof(view_id)='text' AND LENGTH(CAST(view_id AS BLOB))=32 THEN view_id END,CASE WHEN typeof(doc)='text' AND LENGTH(CAST(doc AS BLOB))=32 THEN doc END,gen,CASE WHEN typeof(pane_id)='text' AND LENGTH(CAST(pane_id AS BLOB))=32 THEN pane_id END,CASE WHEN typeof(group_id)='text' AND LENGTH(CAST(group_id AS BLOB))=32 THEN group_id END,position,CASE WHEN typeof(view_state)='text' AND LENGTH(CAST(view_state AS BLOB))<=65536 THEN CAST(view_state AS BLOB) END FROM closed_views WHERE close_id=?1",[sql_rev(close_id)?],|row|Ok((row.get(0)?,row.get(1)?,sql_unsigned(row,2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?)))?;
    let document = document_stub(conn, &id)?;
    if document.lifecycle_gen.0 != generation {
        return Err(error(
            ErrorKind::Stale,
            "closed placement lifecycle changed",
        ));
    }
    Ok(ClosedViewRecord {
        close_id,
        previous_view: LayoutView {
            id: ViewId::from_key(&view)?,
            document: document.key(),
            group_id: GroupId::from_key(&group)?,
            view: serde_json::from_slice(&json.ok_or_else(|| {
                error(
                    ErrorKind::Corrupt,
                    "closed view state is not bounded UTF-8 JSON text",
                )
            })?)?,
        },
        pane_id: PaneId::from_key(&pane)?,
        position,
        document,
    })
}
fn closed_for_document(
    conn: &Connection,
    id: &str,
) -> Result<Option<ClosedViewRecord>, ServiceError> {
    let close: Option<u64> = conn
        .query_row(
            "SELECT close_id FROM closed_views WHERE doc=?1",
            [id],
            |r| sql_unsigned(r, 0),
        )
        .optional()?;
    close.map(|id| read_closed(conn, id)).transpose()
}
pub(super) fn recently_closed(
    conn: &Connection,
    before: Option<u64>,
    limit: u16,
) -> Result<ClosedViewPage, ServiceError> {
    validate_page_limit(limit)?;
    let mut stmt = conn.prepare("SELECT c.close_id FROM closed_views c JOIN documents d ON d.id=c.doc AND d.gen=c.gen AND d.state=1 WHERE c.close_id<?1 ORDER BY c.close_id DESC LIMIT ?2")?;
    let ids = stmt
        .query_map(
            params![
                before.map(sql_rev).transpose()?.unwrap_or(i64::MAX),
                limit as i64 + 1
            ],
            |r| sql_unsigned(r, 0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let next_before_close_id = if ids.len() > limit as usize {
        ids.get(limit as usize - 1).copied()
    } else {
        None
    };
    let mut entries = Vec::new();
    for id in ids.into_iter().take(limit as usize) {
        if let Ok(record) = read_closed(conn, id) {
            entries.push(record);
        }
    }
    Ok(ClosedViewPage {
        next_before_close_id,
        entries,
    })
}
fn insert_document(conn: &Connection, record: &DocumentMetaRecord) -> Result<(), ServiceError> {
    if record.key.lifecycle_gen != LifecycleGen(1) {
        return Err(error(
            ErrorKind::InvalidInput,
            "new document must begin at lifecycle one",
        ));
    }
    let json = serde_json::to_string(&record.meta)?;
    if json.len() > MAX_METADATA_DECODE_BYTES {
        return Err(error(
            ErrorKind::InvalidInput,
            "metadata exceeds decode bound",
        ));
    }
    conn.execute(
        "INSERT INTO documents(id,gen,state,meta) VALUES(?1,?2,0,?3)",
        params![
            record.key.doc_id.key(),
            sql_rev(record.key.lifecycle_gen.0)?,
            json
        ],
    )?;
    Ok(())
}
pub(super) fn write_session(
    conn: &mut Connection,
    write: &SessionWrite,
    meta_seq: u64,
    mailbox: &Mutex<Mailbox>,
    faults: &FaultInjector,
) -> Result<LayoutAck, ServiceError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    expect_head(&tx, write.expected_revision)?;
    let (layout, shared) = match &write.layout {
        LayoutPayload::TreeOnly(tree) => {
            if !write.document_changes.is_empty() {
                return Err(error(
                    ErrorKind::InvalidInput,
                    "tree-only metadata mutation rejected",
                ));
            }
            let (mut current, set) = load_layout(&tx, write.expected_revision)?;
            current.tree = tree.clone();
            (current, Some(set))
        }
        LayoutPayload::Structural(layout) => (layout.clone(), None),
    };
    let mut changed = HashSet::new();
    for change in &write.document_changes {
        let key = change.key();
        let id = key.doc_id.key();
        if !changed.insert(key.doc_id) {
            return Err(error(
                ErrorKind::InvalidInput,
                "duplicate document metadata intent",
            ));
        }
        let previous: Option<(u64, i64)> = tx
            .query_row("SELECT gen,state FROM documents WHERE id=?1", [&id], |r| {
                Ok((sql_unsigned(r, 0)?, r.get(1)?))
            })
            .optional()?;
        match change {
            DocumentChange::Create(record) | DocumentChange::Update(record) => {
                if previous.is_none() {
                    if !matches!(change, DocumentChange::Create(_)) {
                        return Err(error(
                            ErrorKind::Stale,
                            "metadata update has no live document",
                        ));
                    }
                    insert_document(&tx, record)?;
                } else {
                    if previous != Some((key.lifecycle_gen.0, 0)) {
                        return Err(error(
                            ErrorKind::Stale,
                            "metadata cannot reopen a closed or discarded document",
                        ));
                    }
                    let preserve = mailbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .has_newer_meta(key.doc_id, key.lifecycle_gen, meta_seq);
                    let meta = meta_for_source(&tx, &id, &record.meta, preserve)?;
                    let json = serde_json::to_string(&meta)?;
                    if json.len() > MAX_METADATA_DECODE_BYTES {
                        return Err(error(
                            ErrorKind::InvalidInput,
                            "metadata exceeds decode bound",
                        ));
                    }
                    tx.execute(
                        "UPDATE documents SET meta=?2 WHERE id=?1",
                        params![id, json],
                    )?;
                }
            }
            DocumentChange::Reopen(_) => {
                if previous != Some((key.lifecycle_gen.0, 1)) {
                    return Err(error(
                        ErrorKind::Stale,
                        "reopen requires the exact closed document lifecycle",
                    ));
                }
                tx.execute("UPDATE documents SET state=0 WHERE id=?1", [&id])?;
                tx.execute("DELETE FROM closed_views WHERE doc=?1", [&id])?;
            }
        }
    }
    validate_references(&tx, &layout)?;
    if let Some(source) = write.preserve_source_layout {
        pin(&tx, source, "deferred_by_early_session_capacity")?;
    }
    let revision = install_layout(&tx, &layout, shared)?;
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    Ok(LayoutAck {
        sequence: write.sequence,
        revision,
    })
}
fn tombstone(conn: &Connection, key: DocumentKey, next: LifecycleGen) -> Result<(), ServiceError> {
    if next <= key.lifecycle_gen {
        return Err(error(
            ErrorKind::InvalidInput,
            "tombstone lifecycle must be strictly newer",
        ));
    }
    let id = key.doc_id.key();
    let changed = conn.execute(
        "UPDATE documents SET state=2,gen=?3 WHERE id=?1 AND gen=?2 AND state=0",
        params![id, sql_rev(key.lifecycle_gen.0)?, sql_rev(next.0)?],
    )?;
    if changed != 1 {
        return Err(error(ErrorKind::Stale, "tombstone compare-and-swap failed"));
    }
    delete_content(conn, &id)?;
    conn.execute("DELETE FROM closed_views WHERE doc=?1", [&id])?;
    Ok(())
}
fn delete_content(conn: &Connection, id: &str) -> Result<(), ServiceError> {
    conn.execute("DELETE FROM recovery_heads WHERE doc=?1", [id])?;
    conn.execute("DELETE FROM checkpoint_chunks WHERE doc=?1", [id])?;
    conn.execute("DELETE FROM edit_transactions WHERE doc=?1", [id])?;
    Ok(())
}
fn require_durable(conn: &Connection, entry: &CloseEntry) -> Result<(), ServiceError> {
    let id = entry.target.document.doc_id.key();
    let durable: Option<u64> = conn
        .query_row(
            "SELECT durable_rev FROM recovery_heads WHERE doc=?1",
            [&id],
            |r| sql_unsigned(r, 0),
        )
        .optional()?;
    let Some(required) = entry.required_durable_rev else {
        return if durable.is_none() {
            Ok(())
        } else {
            Err(error(
                ErrorKind::Stale,
                "unexpected checkpoint; close protection must be refreshed",
            ))
        };
    };
    // A legacy checkpoint at revision zero is a real checkpoint, not an unknown revision.
    if required.0 == 0 && durable.is_none() {
        return Ok(());
    }
    if durable.is_some_and(|rev| rev >= required.0) {
        return Ok(());
    }
    let (saved_rev, json): (i64,Option<String>) = conn.query_row("SELECT saved_rev,CASE WHEN LENGTH(CAST(saved_meta AS BLOB))<=65536 THEN saved_meta END FROM documents WHERE id=?1", [&id], |r| Ok((r.get(0)?,r.get(1)?)))?;
    let proof = json.and_then(|json| serde_json::from_str::<SaveIntent>(&json).ok());
    let meta = document_stub(conn, &id)?.meta;
    let valid = proof.as_ref().is_some_and(|p| {
        saved_rev == required.0 as i64
            && p.stage == SaveStage::Published
            && p.doc_id == entry.target.document.doc_id
            && p.lifecycle_gen == entry.target.document.lifecycle_gen
            && p.captured_rev == required
            && p.captured_identity.byte_len > DOC_LIMIT as u64
            && entry.expected_identity == Some(p.captured_identity)
            && p.captured_encoding == Some(entry.expected_format.0)
            && p.captured_line_ending == Some(entry.expected_format.1)
            && meta.encoding == entry.expected_format.0
            && meta.line_ending == entry.expected_format.1
            && meta.saved_identity == Some(p.captured_identity)
            && meta.saved_encoding == p.captured_encoding
            && meta.saved_line_ending == p.captured_line_ending
            && meta.path.as_ref() == Some(&p.target_path)
            && p.output_fingerprint.is_some()
            && meta.origin_fingerprint == p.output_fingerprint
    });
    if !valid {
        return Err(error(
            ErrorKind::Storage,
            "frozen revision is not durable and lacks the exact published oversized-save identity proof",
        ));
    }
    delete_content(conn, &id)
}
pub(super) fn close_views(
    conn: &mut Connection,
    batch: &CloseBatch,
    faults: &FaultInjector,
) -> Result<CloseAck, ServiceError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    expect_head(&tx, batch.expected_revision)?;
    let (before, _) = load_layout(&tx, batch.expected_revision)?;
    validate_references(&tx, &before)?;
    if batch.entries.is_empty() {
        return Err(error(ErrorKind::InvalidInput, "empty close batch"));
    }
    let mut seen = HashSet::new();
    let mut expected = before.clone();
    for entry in &batch.entries {
        before.resolve(entry.target)?;
        if !seen.insert(entry.target.view_id) {
            return Err(error(ErrorKind::InvalidInput, "duplicate close target"));
        }
    }
    expected.remove_views(&batch.entries.iter().map(|e| e.target).collect::<Vec<_>>())?;
    if let Some(scratch) = &batch.empty_pane_document {
        if !expected.views.is_empty() {
            return Err(error(
                ErrorKind::InvalidInput,
                "scratch may only replace the final document of an empty session",
            ));
        }
        let view = batch
            .after
            .views
            .values()
            .find(|v| v.document == scratch.key)
            .ok_or_else(|| error(ErrorKind::InvalidInput, "missing scratch view"))?;
        if view.group_id != GroupId::UNGROUPED || view.view != ViewState::default() {
            return Err(error(
                ErrorKind::InvalidInput,
                "scratch must be empty and Ungrouped",
            ));
        }
        expected.insert_view(expected.active_pane_id(), view.clone(), None, true)?;
        insert_document(&tx, scratch)?;
    }
    if expected != batch.after {
        return Err(error(
            ErrorKind::InvalidInput,
            "close after-layout changes unrelated state",
        ));
    }
    let mut closed = Vec::new();
    for entry in &batch.entries {
        let view = before.resolve(entry.target)?;
        let id = view.document.doc_id.key();
        match entry.disposition {
            CloseDisposition::Discard { tombstone_gen } => {
                tombstone(&tx, view.document, tombstone_gen)?
            }
            CloseDisposition::KeepRecovery => {
                require_durable(&tx, entry)?;
                let stub = document_stub(&tx, &id)?;
                let pane = before
                    .pane_for_view(view.id)
                    .expect("validated view placement");
                let position = before
                    .pane(pane)
                    .unwrap()
                    .tabs
                    .iter()
                    .take_while(|id| **id != view.id)
                    .filter(|id| before.views[id].group_id == view.group_id)
                    .count() as u16;
                tx.execute("DELETE FROM closed_views WHERE doc=?1", [&id])?;
                tx.execute("INSERT INTO closed_views(view_id,doc,gen,pane_id,group_id,position,view_state,closed_order) VALUES(?1,?2,?3,?4,?5,?6,?7,COALESCE((SELECT MAX(closed_order)+1 FROM closed_views),1))", params![view.id.key(),id,sql_rev(view.document.lifecycle_gen.0)?,pane.key(),view.group_id.key(),position,serde_json::to_string(&entry.view)?])?;
                let close_id = tx.last_insert_rowid() as u64;
                let changed = tx.execute(
                    "UPDATE documents SET state=1 WHERE id=?1 AND gen=?2 AND state=0",
                    params![id, sql_rev(view.document.lifecycle_gen.0)?],
                )?;
                if changed != 1 {
                    return Err(error(ErrorKind::Stale, "close compare-and-swap failed"));
                }
                closed.push(ClosedViewRecord {
                    close_id,
                    previous_view: LayoutView {
                        view: entry.view.clone(),
                        ..view.clone()
                    },
                    pane_id: pane,
                    position,
                    document: stub,
                });
            }
        }
    }
    validate_references(&tx, &batch.after)?;
    let revision = install_layout(&tx, &batch.after, None)?;
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    Ok(CloseAck {
        layout: LayoutAck {
            sequence: batch.sequence,
            revision,
        },
        closed,
    })
}
pub(super) fn replace_view_document(
    conn: &mut Connection,
    replacement: &ReloadReplacement,
    faults: &FaultInjector,
) -> Result<LayoutAck, ServiceError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    expect_head(&tx, replacement.expected_revision)?;
    let (mut expected, _) = load_layout(&tx, replacement.expected_revision)?;
    expected.resolve(replacement.target)?;
    let staged = replacement
        .after
        .view(replacement.target.view_id)
        .ok_or_else(|| error(ErrorKind::InvalidInput, "reload view missing"))?;
    if staged.document != replacement.replacement.key
        || staged.document.doc_id == replacement.target.document.doc_id
    {
        return Err(error(
            ErrorKind::InvalidInput,
            "reload must bind a fresh document",
        ));
    }
    let old = expected.views.get_mut(&replacement.target.view_id).unwrap();
    old.document = staged.document;
    old.view = staged.view.clone();
    if expected != replacement.after {
        return Err(error(
            ErrorKind::InvalidInput,
            "reload changes unrelated layout state",
        ));
    }
    let meta = &replacement.replacement.meta;
    if meta.path.is_none()
        || meta.origin_fingerprint.is_none()
        || (!meta.paged
            && (meta.saved_identity.is_none()
                || meta.saved_encoding != Some(meta.encoding)
                || meta.saved_line_ending != Some(meta.line_ending)))
    {
        return Err(error(
            ErrorKind::InvalidInput,
            "reload requires verified clean-disk metadata",
        ));
    }
    let prior: Option<i64> = tx
        .query_row(
            "SELECT gen FROM documents WHERE id=?1",
            [replacement.target.document.doc_id.key()],
            |row| row.get(0),
        )
        .optional()?;
    if prior.is_none() {
        if replacement.target.document.lifecycle_gen != LifecycleGen(1)
            || replacement.tombstone_gen <= LifecycleGen(1)
        {
            return Err(error(
                ErrorKind::Stale,
                "missing reload source is only admissible at initial lifecycle one",
            ));
        }
        let id = replacement.target.document.doc_id.key();
        tx.execute(
            "INSERT INTO documents(id,gen,state,meta) VALUES(?1,?2,2,?3)",
            params![
                id,
                sql_rev(replacement.tombstone_gen.0)?,
                serde_json::to_string(&DocMeta::default())?
            ],
        )?;
        delete_content(&tx, &id)?;
        tx.execute("DELETE FROM closed_views WHERE doc=?1", [&id])?;
    } else {
        tombstone(&tx, replacement.target.document, replacement.tombstone_gen)?;
    }
    insert_document(&tx, &replacement.replacement)?;
    validate_references(&tx, &replacement.after)?;
    let revision = install_layout(&tx, &replacement.after, None)?;
    faults.check(FaultPoint::RecoveryBeforeCommit)?;
    tx.commit()?;
    Ok(LayoutAck {
        sequence: replacement.sequence,
        revision,
    })
}
