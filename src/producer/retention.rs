//! One disposable, SQL-published last successful run per producer (#632).
//!
//! The index writer attaches this database to its rollback-journal transaction. A slot
//! is therefore publication truth, including a crash before the request's terminal ACK.
use super::state::StateDir;
use crate::{
    semantic_batch::descriptor::GenerationOrigin, store::requests::GeneratedImportOptions,
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

pub(crate) fn unavailable_catalog(error: &anyhow::Error) -> bool {
    error.to_string().starts_with(super::PRODUCER_STATE_CORRUPT)
        || error.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
            matches!(error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase))
        })
}

fn decode<T: serde::de::DeserializeOwned>(json: &str) -> Result<T> {
    serde_json::from_str(json).map_err(|error| {
        anyhow::anyhow!("{}: retained run: {error}", super::PRODUCER_STATE_CORRUPT)
    })
}

pub(crate) fn inputs_present(options: &GeneratedImportOptions) -> Result<bool> {
    match std::fs::symlink_metadata(&options.descriptor) {
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "{}: retained descriptor is not a regular file",
                super::PRODUCER_STATE_CORRUPT
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub const FILE: &str = "retention.sqlite";
const SCHEMA: &str = "CREATE TABLE identity(root_key TEXT NOT NULL); CREATE TABLE retained_runs(producer TEXT PRIMARY KEY,languages TEXT NOT NULL,options TEXT NOT NULL,descriptor_sha256 TEXT NOT NULL,source_revision TEXT NOT NULL,attempted_generation TEXT); CREATE TABLE legacy_languages(language TEXT PRIMARY KEY); PRAGMA user_version=1;";

pub(crate) fn attach(db: &Connection, state: &StateDir, root_key: &str) -> Result<()> {
    attach_inner(db, state, root_key).map_err(|error| {
        if unavailable_catalog(&error) {
            anyhow::anyhow!(
                "{}: retention catalog: {error}",
                super::PRODUCER_STATE_CORRUPT
            )
        } else {
            error
        }
    })
}

fn attach_inner(db: &Connection, state: &StateDir, root_key: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = state.sqlite_path(FILE)?;
    let new = !path.try_exists()?;
    // Index connections deliberately lack SQLITE_OPEN_CREATE. ATTACH inherits
    // that flag, so provision the disposable catalog through SQLite first.
    {
        let mut catalog = Connection::open(&path)?;
        if new {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        catalog.busy_timeout(Duration::from_secs(5))?;
        catalog.pragma_update(None, "synchronous", "FULL")?;
        let tx = catalog.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version == 0 {
            tx.execute_batch(SCHEMA)?;
            tx.execute("INSERT INTO identity VALUES(?1)", [root_key])?;
        }
        ensure!(
            version == 0 || version == 1,
            "producer_state_corrupt: retention schema"
        );
        let identity: String =
            tx.query_row("SELECT root_key FROM identity", [], |row| row.get(0))?;
        ensure!(
            identity == root_key,
            "producer_state_corrupt: retention root"
        );
        tx.commit()?;
    }
    db.execute(
        "ATTACH DATABASE ?1 AS retained",
        [path.to_string_lossy().as_ref()],
    )?;
    for schema in ["main", "retained"] {
        let mode: String = db.pragma_query_value(Some(schema), "journal_mode", |row| row.get(0))?;
        ensure!(
            mode.eq_ignore_ascii_case("delete"),
            "producer_state_corrupt: retention requires rollback journals"
        );
        db.pragma_update(Some(schema), "synchronous", "FULL")?;
    }
    slots(db)?;
    Ok(())
}

/// Missing state has no hold; unreadable state is an error and cleanup must defer.
pub(crate) fn held_dirs(state: &StateDir) -> Result<BTreeSet<PathBuf>> {
    let path = state.sqlite_path(FILE)?;
    if !path.try_exists()? {
        return Ok(BTreeSet::new());
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(250))?;
    let mut statement = db.prepare("SELECT options FROM retained_runs ORDER BY producer")?;
    let options = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    options
        .into_iter()
        .map(|json| {
            let options: GeneratedImportOptions = decode(&json)?;
            ensure!(
                options.origin == GenerationOrigin::TrellisRun,
                "producer_state_corrupt: retained origin"
            );
            state.run_dir(&options.generation_id)
        })
        .collect()
}

pub(crate) fn publish_run(
    db: &Connection,
    state: &StateDir,
    descriptor: &crate::semantic_batch::descriptor::Descriptor,
    digest: &str,
) -> Result<()> {
    if let Some(generation) = &descriptor.generation
        && generation.origin == GenerationOrigin::TrellisRun
    {
        // The exact original location is part of admission, never a relocated capture.
        let options = GeneratedImportOptions {
            version: 2,
            descriptor: state
                .run_dir(&generation.generation_id)?
                .canonicalize()?
                .join("batch/descriptor.json")
                .to_string_lossy()
                .into_owned(),
            descriptor_sha256: digest.to_owned(),
            origin: generation.origin,
            generation_id: generation.generation_id.clone(),
        };
        let producer = descriptor.check_producer()?.producer();
        db.execute("INSERT INTO retained.retained_runs VALUES(?1,?2,?3,?4,?5,NULL) ON CONFLICT(producer) DO UPDATE SET languages=excluded.languages,options=excluded.options,descriptor_sha256=excluded.descriptor_sha256,source_revision=excluded.source_revision,attempted_generation=NULL",
            params![producer.id, serde_json::to_string(&descriptor.producer.languages)?,serde_json::to_string(&options)?,digest,descriptor.source_revision])?;
    }
    for language in &descriptor.producer.languages {
        db.execute(
            "DELETE FROM retained.legacy_languages WHERE language=?1",
            [language],
        )?;
    }
    Ok(())
}

pub(crate) struct RetainedRun {
    pub producer: String,
    pub languages: Vec<String>,
    pub options: GeneratedImportOptions,
    pub attempted_generation: Option<String>,
}

pub(crate) fn slots(db: &Connection) -> Result<Vec<RetainedRun>> {
    let mut statement = db.prepare("SELECT producer,languages,options,attempted_generation FROM retained.retained_runs ORDER BY producer")?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(producer, languages, options, attempted_generation)| {
            Ok(RetainedRun {
                producer,
                languages: decode(&languages)?,
                options: decode(&options)?,
                attempted_generation,
            })
        })
        .collect()
}

/// Writer-only diagnosis from metadata committed with the original successful admission.
pub(crate) fn recovery_reason(
    db: &Connection,
    key: &str,
    options: &GeneratedImportOptions,
) -> Result<String> {
    let revision: String = db.query_row(
        "SELECT native_revision_id FROM native_revisions WHERE id=?1",
        [key],
        |row| row.get(0),
    )?;
    let captured: String = db.query_row(
        "SELECT source_revision FROM retained.retained_runs WHERE descriptor_sha256=?1",
        [&options.descriptor_sha256],
        |row| row.get(0),
    )?;
    Ok(if !inputs_present(options)? {
        "retained run inputs are missing; import a current artifact".into()
    } else if revision != captured {
        "retained run native revision no longer matches; import a current artifact".into()
    } else {
        "retained run awaits normal validated re-import".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
    use std::fs;

    fn world() -> (tempfile::TempDir, StateDir, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let state = StateDir::open(&roots, &identity).unwrap();
        (temp, state, identity.record_id.clone())
    }
    fn insert(db: &Connection, state: &StateDir, id: &str) {
        let options = GeneratedImportOptions {
            version: 2,
            descriptor: state
                .run_dir(id)
                .unwrap()
                .join("batch/descriptor.json")
                .to_string_lossy()
                .into_owned(),
            descriptor_sha256: "a".repeat(64),
            origin: GenerationOrigin::TrellisRun,
            generation_id: id.into(),
        };
        db.execute("INSERT OR REPLACE INTO retained.retained_runs VALUES('producer','[\"rust\"]',?1,?2,'native',NULL)",params![serde_json::to_string(&options).unwrap(),"a".repeat(64)]).unwrap();
    }
    #[test]
    fn retention_authority_is_atomic_with_index_commit_before_ack() {
        let (temp, state, key) = world();
        let id1 = "1".repeat(32);
        let id2 = "2".repeat(32);
        state.create_run_dir(&id1).unwrap();
        state.create_run_dir(&id2).unwrap();
        let mut db = Connection::open(temp.path().join("index.db")).unwrap();
        db.execute_batch("CREATE TABLE published(id TEXT)").unwrap();
        attach(&db, &state, &key).unwrap();
        {
            let tx = db.transaction().unwrap();
            tx.execute("INSERT INTO published VALUES(?1)", [&id1])
                .unwrap();
            insert(&tx, &state, &id1);
            tx.commit().unwrap();
        }
        // A failed later attempt cannot replace the successful hold.
        {
            let tx = db.transaction().unwrap();
            tx.execute("INSERT INTO published VALUES(?1)", [&id2])
                .unwrap();
            insert(&tx, &state, &id2);
        }
        drop(db);
        assert_eq!(
            held_dirs(&state).unwrap(),
            BTreeSet::from([state.run_dir(&id1).unwrap()])
        );
        // Simulate restart before any terminal request/run record was written.
        let mut db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        {
            let tx = db.transaction().unwrap();
            tx.execute("INSERT INTO published VALUES(?1)", [&id2])
                .unwrap();
            insert(&tx, &state, &id2);
            tx.commit().unwrap();
        }
        drop(db);
        assert_eq!(
            held_dirs(&state).unwrap(),
            BTreeSet::from([state.run_dir(&id2).unwrap()])
        );
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM published", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn retention_cleanup_preserves_one_run_and_releases_superseded_inputs() {
        let (temp, state, key) = world();
        let id1 = "1".repeat(32);
        let id2 = "2".repeat(32);
        for id in [&id1, &id2] {
            let dir = state.create_run_dir(id).unwrap();
            for name in ["out", "batch", "worktree"] {
                fs::create_dir(dir.join(name)).unwrap();
            }
        }
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        insert(&db, &state, &id1);
        drop(db);
        state.strip_to_record(&id1).unwrap();
        state.strip_to_record(&id2).unwrap();
        assert!(state.run_dir(&id1).unwrap().join("batch").exists());
        assert!(!state.run_dir(&id1).unwrap().join("worktree").exists());
        assert!(!state.run_dir(&id2).unwrap().join("batch").exists());
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        insert(&db, &state, &id2);
        drop(db);
        state.strip_to_record(&id1).unwrap();
        assert!(!state.run_dir(&id1).unwrap().join("batch").exists());
    }
    #[test]
    fn missing_capture_is_typed_without_invalidating_native_head() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        let store = crate::store::Store::open_for_tests(temp.path(), &root).unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) = crate::indexer::index_workspace_bundle(
            &crate::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let session = store.writer_session().unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                session.writer_guard().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let error = store
            .retained_native_pin_for("missing-revision", "missing-source-set")
            .unwrap_err();
        assert!(
            error.to_string().starts_with("capture_unavailable:"),
            "{error:#}"
        );
        assert!(!store.recovery_pending_for_tests());
        assert_eq!(store.status().unwrap().revision, pin);
        assert_eq!(store.status().unwrap().stats, graph.stats);
    }

    #[test]
    fn corrupt_retention_defers_cleanup_without_deleting_inputs() {
        let (_temp, state, _key) = world();
        let id = "3".repeat(32);
        let dir = state.create_run_dir(&id).unwrap();
        fs::create_dir(dir.join("batch")).unwrap();
        fs::write(state.path().join(FILE), b"broken sqlite").unwrap();
        assert!(state.strip_to_record(&id).is_err());
        assert!(dir.join("batch").exists());
        assert!(
            state
                .sweep(
                    &super::super::state::DaemonIdentity::current(),
                    &BTreeSet::new()
                )
                .is_err()
        );
        assert!(dir.exists());
        assert!(
            decode::<GeneratedImportOptions>("broken JSON")
                .unwrap_err()
                .to_string()
                .starts_with(super::super::PRODUCER_STATE_CORRUPT)
        );
        fs::remove_file(state.path().join(FILE)).unwrap();
        let elsewhere = state.path().join("elsewhere.sqlite");
        fs::write(&elsewhere, b"").unwrap();
        std::os::unix::fs::symlink(&elsewhere, state.path().join(FILE)).unwrap();
        assert!(
            held_dirs(&state)
                .unwrap_err()
                .to_string()
                .starts_with(super::super::UNSAFE_STATE)
        );
        assert!(dir.join("batch").exists());
    }
}
