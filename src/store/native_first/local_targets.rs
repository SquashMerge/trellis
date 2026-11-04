//! One target-owned, disposable LOCAL declaration index. This is not source proof.
//! The owner retains its Stage across producers, including bounded FULL fallbacks.
use super::witness::{self, CurrentDeclaration, SelectedVersion};
use super::*;
use crate::native_evidence::{DocumentKey, Range};
use crate::scip_raw::ScipCaptureError;
use crate::semantic_batch::{BatchLimits, staging::Stage};

#[derive(Default)]
pub(crate) struct TargetIndexCursor {
    captured: Option<IndexPin>,
    after: Option<(String, String)>,
    active: Option<(DocumentKey, SelectedVersion, Vec<u8>)>,
    done: bool,
}

#[derive(Default)]
// Work fields feed the test-support cost counters only.
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) struct TargetAdvance {
    pub done: bool,
    pub builds: usize,
    pub changed_documents: usize,
    pub current_target_rows: usize,
}

// Work fields feed the test-support cost counters only.
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub(crate) struct TargetLookup {
    pub declaration: Option<CurrentDeclaration>,
    /// Actual normalized native declaration rows fetched, not probes or cache hits.
    pub rows: usize,
    pub read: bool,
}

const SCHEMA: &str = "
CREATE TABLE local_target_owner(singleton INTEGER PRIMARY KEY CHECK(singleton=1),pin TEXT NOT NULL,rows INTEGER NOT NULL DEFAULT 0);
CREATE TABLE local_target_documents(pin TEXT NOT NULL,source_set TEXT NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,vid INTEGER NOT NULL,complete INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(pin,source_set,language,path)) WITHOUT ROWID;
CREATE TABLE local_target_declarations(pin TEXT NOT NULL,source_set TEXT NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,present INTEGER NOT NULL,kind TEXT,name TEXT,name_start INTEGER,name_end INTEGER,callable INTEGER,PRIMARY KEY(pin,source_set,language,path,syntax_id)) WITHOUT ROWID;
";

/// Stage bytes charged for one cached lookup row.
const LOOKUP_STAGE_BYTES: u64 = 512;

fn corrupt(message: &str) -> anyhow::Error {
    SelectedIntegrity(format!("incompatible_index: {message}")).into()
}
fn limit(name: &str) -> anyhow::Error {
    ScipCaptureError::LimitExceeded(name.into()).into()
}

fn initialize(stage: &mut Stage, pin: IndexPin) -> Result<bool> {
    let conn = stage.conn()?;
    let exists: bool = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='local_target_owner')")?.query_row( [], |r| r.get(0))?;
    if !exists {
        conn.execute_batch(SCHEMA)?;
        conn.execute(
            "INSERT INTO local_target_owner(singleton,pin) VALUES(1,?1)",
            [semantic_pin_key(pin)],
        )?;
    }
    let owner: String = conn
        .prepare_cached("SELECT pin FROM local_target_owner WHERE singleton=1")?
        .query_row([], |r| r.get(0))?;
    selected_ensure!(owner == semantic_pin_key(pin), "LOCAL index target changed");
    Ok(!exists)
}

fn reserve_row(row: &rusqlite::Row<'_>, limits: &BatchLimits) -> Result<()> {
    let mut bytes = std::mem::size_of::<CurrentDeclaration>() as u64;
    for i in 0..row.as_ref().column_count() {
        let size = match row.get_ref(i)? {
            rusqlite::types::ValueRef::Text(v) | rusqlite::types::ValueRef::Blob(v) => {
                v.len() as u64
            }
            _ => 0,
        };
        bytes = bytes
            .checked_add(size.checked_mul(2).ok_or_else(|| limit("unit expansion"))?)
            .ok_or_else(|| limit("unit expansion"))?;
    }
    if bytes > limits.unit_expansion_bytes {
        return Err(limit("unit expansion"));
    }
    Ok(())
}

fn account(conn: &Connection, limits: &BatchLimits) -> Result<()> {
    let rows: i64 = conn
        .prepare_cached("SELECT rows FROM local_target_owner WHERE singleton=1")?
        .query_row([], |r| r.get(0))?;
    if rows < 0 || rows as u64 >= limits.staged_rows {
        return Err(limit("staged rows"));
    }
    conn.execute(
        "UPDATE local_target_owner SET rows=rows+1 WHERE singleton=1",
        [],
    )?;
    Ok(())
}

/// Advance one inventory document or one bounded declaration page. A cursor belongs
/// to one captured pin; completed N documents are shared across all such cursors.
pub(crate) fn advance(
    db: &Connection,
    target: &PendingSemanticTarget,
    captured_pin: IndexPin,
    stage: &mut Stage,
    cursor: &mut TargetIndexCursor,
    limits: &BatchLimits,
    cancel: &CancelFlag,
) -> Result<TargetAdvance> {
    check_cancel(cancel)?;
    selected_ensure!(
        pending_target(db)?.as_ref() == Some(target),
        "LOCAL target changed"
    );
    selected_ensure!(
        target
            .selections
            .iter()
            .any(|s| s.captured_pin == captured_pin),
        "LOCAL captured pin not selected"
    );
    if let Some(pin) = cursor.captured {
        selected_ensure!(pin == captured_pin, "LOCAL captured cursor changed");
    } else {
        cursor.captured = Some(captured_pin);
    }
    let mut work = TargetAdvance {
        builds: initialize(stage, target.pin)? as usize,
        ..Default::default()
    };
    let conn = stage.conn()?;
    if cursor.done {
        work.done = true;
        return Ok(work);
    }
    if let Some((key, version, after)) = cursor.active.as_mut() {
        selected_ensure!(
            witness::selected_version(db, target.pin, key)?.as_ref() == Some(&*version),
            "LOCAL selected version changed"
        );
        if limits.page_rows == 0 {
            return Err(limit("page rows"));
        }
        let mut stmt = db.prepare("SELECT syntax_id,kind,name,name_start,name_end,start_byte,end_byte FROM native_version_declarations WHERE vid=?1 AND syntax_id>?2 ORDER BY syntax_id LIMIT ?3")?;
        let mut rows = stmt.query(params![
            version.vid,
            after.as_slice(),
            i64::try_from(limits.page_rows).unwrap_or(i64::MAX)
        ])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            check_cancel(cancel)?;
            reserve_row(row, limits)?;
            let encoded: Vec<u8> = row.get(0)?;
            id_codec::decode_sid(&encoded).map_err(|_| corrupt("invalid LOCAL declaration ID"))?;
            let decl = decode_declaration(row, version.clone())?;
            put(
                conn,
                target.pin,
                key,
                version.vid,
                &encoded,
                Some(&decl),
                limits,
            )?;
            *after = encoded;
            count += 1;
        }
        work.current_target_rows = count as usize;
        if count < limits.page_rows {
            conn.execute("UPDATE local_target_documents SET complete=1 WHERE pin=?1 AND source_set=?2 AND language=?3 AND path=?4 AND vid=?5", params![semantic_pin_key(target.pin), key.source_set_id, key.language, key.path, version.vid])?;
            cursor.active = None;
        }
    } else {
        let source_set: String = db
            .prepare_cached("SELECT source_set_id FROM native_revisions WHERE id=?1")?
            .query_row([semantic_pin_key(target.pin)], |r| r.get(0))?;
        let (after_language, after_path) = cursor
            .after
            .as_ref()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .unwrap_or(("", ""));
        let mut stmt = db.prepare("SELECT language,path FROM revision_documents WHERE revision_id=?1 AND (language,path)>(?2,?3) ORDER BY language,path LIMIT 1")?;
        let mut rows = stmt.query(params![
            semantic_pin_key(target.pin),
            after_language,
            after_path
        ])?;
        if let Some(row) = rows.next()? {
            reserve_row(row, limits)?;
            let key = DocumentKey {
                source_set_id: source_set,
                language: row.get(0)?,
                path: row.get(1)?,
            };
            let version = witness::selected_version(db, target.pin, &key)?
                .ok_or_else(|| corrupt("LOCAL inventory version absent"))?;
            let captured = witness::selected_version(db, captured_pin, &key)?;
            cursor.after = Some((key.language.clone(), key.path.clone()));
            if captured.as_ref() != Some(&version) {
                let complete: Option<(i64,bool)> = conn.prepare_cached("SELECT vid,complete FROM local_target_documents WHERE pin=?1 AND source_set=?2 AND language=?3 AND path=?4")?.query_row( params![semantic_pin_key(target.pin),key.source_set_id,key.language,key.path], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
                if let Some((vid, _)) = complete {
                    selected_ensure!(vid == version.vid, "LOCAL document version changed");
                }
                if !complete.is_some_and(|(_, done)| done) {
                    if complete.is_none() {
                        account(conn, limits)?;
                        conn.execute("INSERT INTO local_target_documents(pin,source_set,language,path,vid) VALUES(?1,?2,?3,?4,?5)",params![semantic_pin_key(target.pin),key.source_set_id,key.language,key.path,version.vid])?;
                        work.changed_documents = 1;
                    }
                    cursor.active = Some((key, version, Vec::new()));
                }
            }
        } else {
            cursor.done = true;
            work.done = true;
        }
    }
    stage.budget(limits).check()?;
    Ok(work)
}

// Columns 0..6 are the exact scalar projection used by the changed-document page.
fn decode_declaration(
    row: &rusqlite::Row<'_>,
    version: SelectedVersion,
) -> Result<CurrentDeclaration> {
    let (kind, name, start, end, outer_start, outer_end): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
    ) = (
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    );
    let name_range = match (&name, start, end) {
        (None, None, None) => None,
        (Some(_), Some(start), Some(end)) => {
            selected_ensure!(
                outer_start >= 0
                    && start >= outer_start
                    && end > start
                    && outer_end >= end
                    && outer_end as u64 <= version.byte_length,
                "invalid LOCAL declaration span"
            );
            Some(Range {
                start: start as usize,
                end: end as usize,
            })
        }
        _ => return Err(corrupt("malformed LOCAL declaration")),
    };
    let callable = name.is_some() && matches!(kind.as_str(), "function" | "method" | "constructor");
    Ok(CurrentDeclaration {
        kind,
        name,
        name_range,
        callable,
    })
}

fn put(
    conn: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
    vid: i64,
    sid: &[u8],
    declaration: Option<&CurrentDeclaration>,
    limits: &BatchLimits,
) -> Result<()> {
    let pin = semantic_pin_key(pin);
    let exists: bool = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM local_target_declarations WHERE pin=?1 AND source_set=?2 AND language=?3 AND path=?4 AND syntax_id=?5)")?.query_row(params![pin,key.source_set_id,key.language,key.path,sid], |r| r.get(0))?;
    if !exists {
        account(conn, limits)?;
    }
    let (kind, name, start, end, callable) = match declaration {
        Some(d) => (
            Some(d.kind.as_str()),
            d.name.as_deref(),
            d.name_range.as_ref().map(|r| r.start as i64),
            d.name_range.as_ref().map(|r| r.end as i64),
            Some(d.callable),
        ),
        None => (None, None, None, None, None),
    };
    conn.execute("INSERT INTO local_target_declarations(pin,source_set,language,path,vid,syntax_id,present,kind,name,name_start,name_end,callable) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12) ON CONFLICT(pin,source_set,language,path,syntax_id) DO UPDATE SET vid=excluded.vid,present=excluded.present,kind=excluded.kind,name=excluded.name,name_start=excluded.name_start,name_end=excluded.name_end,callable=excluded.callable",params![pin,key.source_set_id,key.language,key.path,vid,sid,declaration.is_some(),kind,name,start,end,callable])?;
    Ok(())
}

/// Exact selected normalized lookup. Both hits and absence are cached under the
/// requested pin and selected vid; only fully indexed documents prove absence.
///
/// A cache miss stages one row; the stage directory is measured once `unchecked` (the
/// caller's running estimate) reaches a MiB, not on every lookup.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lookup(
    db: &Connection,
    pin: IndexPin,
    key: &DocumentKey,
    syntax_id: &str,
    stage: &mut Stage,
    limits: &BatchLimits,
    unchecked: &mut u64,
    cancel: &CancelFlag,
) -> Result<TargetLookup> {
    check_cancel(cancel)?;
    let target = pending_target(db)?.ok_or_else(|| corrupt("LOCAL target absent"))?;
    selected_ensure!(
        pin == target.pin || target.selections.iter().any(|s| s.captured_pin == pin),
        "LOCAL lookup pin not selected"
    );
    initialize(stage, target.pin)?;
    let encoded =
        id_codec::encode_sid(syntax_id).ok_or_else(|| corrupt("invalid LOCAL target syntax ID"))?;
    let version = witness::selected_version(db, pin, key)?;
    let vid = version.as_ref().map_or(0, |v| v.vid);
    let conn = stage.conn()?;
    let mut stmt = conn.prepare("SELECT vid,present,kind,name,name_start,name_end,callable FROM local_target_declarations WHERE pin=?1 AND source_set=?2 AND language=?3 AND path=?4 AND syntax_id=?5")?;
    let mut cached = stmt.query(params![
        semantic_pin_key(pin),
        key.source_set_id,
        key.language,
        key.path,
        encoded
    ])?;
    if let Some(row) = cached.next()? {
        reserve_row(row, limits)?;
        selected_ensure!(
            row.get::<_, i64>(0)? == vid,
            "LOCAL cached selection changed"
        );
        let present: bool = row.get(1)?;
        let declaration = if present {
            version.ok_or_else(|| corrupt("LOCAL cached document absent"))?;
            let start: Option<i64> = row.get(4)?;
            let end: Option<i64> = row.get(5)?;
            Some(CurrentDeclaration {
                kind: row.get(2)?,
                name: row.get(3)?,
                name_range: start.zip(end).map(|(s, e)| Range {
                    start: s as usize,
                    end: e as usize,
                }),
                callable: row.get(6)?,
            })
        } else {
            None
        };
        return Ok(TargetLookup {
            declaration,
            rows: 0,
            read: false,
        });
    }
    drop(cached);
    drop(stmt);
    let complete: bool = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM local_target_documents WHERE pin=?1 AND source_set=?2 AND language=?3 AND path=?4 AND vid=?5 AND complete=1)")?.query_row(params![semantic_pin_key(pin),key.source_set_id,key.language,key.path,vid], |r|r.get(0))?;
    let read = !complete && version.is_some();
    let declaration = if read {
        witness::current_declaration(db, pin, key, syntax_id, &mut |bytes| {
            if bytes > limits.unit_expansion_bytes {
                return Err(limit("unit expansion"));
            }
            Ok(())
        })?
    } else {
        None
    };
    let rows = declaration.is_some() as usize;
    put(conn, pin, key, vid, &encoded, declaration.as_ref(), limits)?;
    stage.budget(limits).charge(unchecked, LOOKUP_STAGE_BYTES)?;
    Ok(TargetLookup {
        declaration,
        rows,
        read,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    fn pin(revision: u64) -> IndexPin {
        IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: revision,
        }
    }
    fn key() -> DocumentKey {
        DocumentKey {
            source_set_id: "source".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        }
    }
    // No source bytes or full native bodies exist in this fixture.
    fn fixture() -> (Connection, PendingSemanticTarget, tempfile::TempDir, Stage) {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("
CREATE TABLE index_metadata(singleton INTEGER,index_generation TEXT,index_revision INTEGER);
CREATE TABLE native_revisions(id TEXT,source_set_id TEXT,native_revision_id TEXT);
CREATE TABLE semantic_pending_target(singleton INTEGER,revision_id TEXT,native_revision_id TEXT,root_id TEXT);
CREATE TABLE semantic_pending_selections(revision_id TEXT,producer_id TEXT,batch_id INTEGER,captured_pin TEXT,basis_pin TEXT);
CREATE TABLE semantic_revision_raw_selections(revision_id TEXT,producer_id TEXT,batch_id INTEGER,captured_pin TEXT);
CREATE TABLE semantic_projection_origins(revision_id TEXT,producer_id TEXT,basis_pin TEXT);
CREATE TABLE revision_documents(revision_id TEXT,language TEXT,path TEXT,vid INTEGER);
CREATE TABLE document_versions(vid INTEGER,id TEXT,language TEXT,path TEXT,content_hash TEXT,byte_length INTEGER,extraction_context TEXT,producer_id TEXT,producer_version TEXT);
CREATE TABLE native_version_declarations(vid INTEGER,syntax_id BLOB,kind TEXT,name TEXT,name_start INTEGER,name_end INTEGER,start_byte INTEGER,end_byte INTEGER);
").unwrap();
        let current = semantic_pin_key(pin(2));
        let captured = semantic_pin_key(pin(1));
        db.execute(
            "INSERT INTO index_metadata VALUES(1,?1,2)",
            [pin(2).index_generation.to_string()],
        )
        .unwrap();
        for (p, native) in [(&captured, "old"), (&current, "new")] {
            db.execute(
                "INSERT INTO native_revisions VALUES(?1,'source',?2)",
                params![p, native],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO semantic_pending_target VALUES(1,?1,'new','root')",
            [&current],
        )
        .unwrap();
        db.execute(
            "INSERT INTO semantic_pending_selections VALUES(?1,'producer',1,?2,?2)",
            params![current, captured],
        )
        .unwrap();
        db.execute(
            "INSERT INTO semantic_revision_raw_selections VALUES(?1,'producer',1,?2)",
            params![current, captured],
        )
        .unwrap();
        db.execute(
            "INSERT INTO semantic_projection_origins VALUES(?1,'producer',?2)",
            params![current, captured],
        )
        .unwrap();
        for (vid, p) in [(1, &captured), (2, &current)] {
            db.execute("INSERT INTO document_versions VALUES(?1,?2,'javascript','a.js',?3,30,?4,'native','1')",params![vid,format!("version-{vid}"),"a".repeat(64),"b".repeat(64)]).unwrap();
            db.execute(
                "INSERT INTO revision_documents VALUES(?1,'javascript','a.js',?2)",
                params![p, vid],
            )
            .unwrap();
        }
        for (vid, kind, name, start, end) in [
            (1, "function", Some("old"), Some(1), Some(4)),
            (2, "method", Some("new"), Some(1), Some(4)),
            (2, "block", None, None, None),
        ] {
            let sid = format!(
                "sid:v1:{}",
                if name.is_some() { "1" } else { "2" }.repeat(32)
            );
            db.execute(
                "INSERT INTO native_version_declarations VALUES(?1,?2,?3,?4,?5,?6,0,30)",
                params![
                    vid,
                    id_codec::encode_sid(&sid).unwrap(),
                    kind,
                    name,
                    start,
                    end
                ],
            )
            .unwrap();
        }
        let target = pending_target(&db).unwrap().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let stage = Stage::create(dir.path(), "local-target", "test").unwrap();
        (db, target, dir, stage)
    }

    #[test]
    fn local_target_pages_are_shared_and_exact_lookups_are_pin_scoped() {
        let (db, target, _dir, mut stage) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let limits = BatchLimits {
            page_rows: 1,
            ..Default::default()
        };
        let mut cursor = TargetIndexCursor::default();
        let (mut builds, mut documents, mut rows) = (0, 0, 0);
        for _ in 0..8 {
            let work = advance(
                &db,
                &target,
                pin(1),
                &mut stage,
                &mut cursor,
                &limits,
                &cancel,
            )
            .unwrap();
            builds += work.builds;
            documents += work.changed_documents;
            rows += work.current_target_rows;
            if work.done {
                break;
            }
        }
        assert!(cursor.done);
        assert_eq!((builds, documents, rows), (1, 1, 2));
        let sid = format!("sid:v1:{}", "1".repeat(32));
        let now = lookup(&db, pin(2), &key(), &sid, &mut stage, &limits, &mut 0, &cancel).unwrap();
        assert_eq!(now.declaration.unwrap().name.as_deref(), Some("new"));
        assert!(!now.read);
        let old = lookup(&db, pin(1), &key(), &sid, &mut stage, &limits, &mut 0, &cancel).unwrap();
        assert_eq!(old.declaration.unwrap().name.as_deref(), Some("old"));
        assert!(old.read);
        assert_eq!(old.rows, 1);
        assert!(
            !lookup(&db, pin(1), &key(), &sid, &mut stage, &limits, &mut 0, &cancel)
                .unwrap()
                .read
        );
        let mut second = TargetIndexCursor::default();
        for _ in 0..3 {
            let work = advance(
                &db,
                &target,
                pin(1),
                &mut stage,
                &mut second,
                &limits,
                &cancel,
            )
            .unwrap();
            assert_eq!(
                (
                    work.builds,
                    work.changed_documents,
                    work.current_target_rows
                ),
                (0, 0, 0)
            );
            if work.done {
                break;
            }
        }
        assert!(second.done);
    }

    #[test]
    fn local_target_exact_lookup_reserves_before_owned_scalar_copy() {
        let (db, _target, _dir, mut stage) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let limits = BatchLimits {
            unit_expansion_bytes: 1,
            ..Default::default()
        };
        let sid = format!("sid:v1:{}", "1".repeat(32));
        let error = lookup(&db, pin(2), &key(), &sid, &mut stage, &limits, &mut 0, &cancel)
            .err()
            .unwrap();
        assert!(error.downcast_ref::<ScipCaptureError>().is_some());
        let count: i64 = stage
            .conn()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM local_target_declarations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn local_target_caches_absence_and_preserves_unnamed_presence() {
        let (db, _target, _dir, mut stage) = fixture();
        let cancel = Arc::new(AtomicBool::new(false));
        let limits = BatchLimits::default();
        let missing = format!("sid:v1:{}", "3".repeat(32));
        let absent = lookup(&db, pin(2), &key(), &missing, &mut stage, &limits, &mut 0, &cancel).unwrap();
        assert!(absent.declaration.is_none() && absent.read && absent.rows == 0);
        assert!(
            !lookup(&db, pin(2), &key(), &missing, &mut stage, &limits, &mut 0, &cancel)
                .unwrap()
                .read
        );
        let unnamed = format!("sid:v1:{}", "2".repeat(32));
        let present = lookup(&db, pin(2), &key(), &unnamed, &mut stage, &limits, &mut 0, &cancel).unwrap();
        assert!(present.declaration.unwrap().name.is_none());
        assert_eq!(present.rows, 1);
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(lookup(&db, pin(2), &key(), &unnamed, &mut stage, &limits, &mut 0, &cancel).is_err());
    }
}
