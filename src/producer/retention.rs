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

/// Only fixed catalog contract queries use this conversion. Operational failures
/// remain retryable/reportable SQLite errors, not optional catalog corruption.
fn catalog_query<T>(result: rusqlite::Result<T>) -> Result<T> {
    result.map_err(|error| match &error {
        rusqlite::Error::QueryReturnedNoRows
        | rusqlite::Error::InvalidColumnType(..)
        | rusqlite::Error::FromSqlConversionFailure(..)
        | rusqlite::Error::IntegralValueOutOfRange(..) => anyhow::anyhow!(
            "{}: retention catalog: {error}",
            super::PRODUCER_STATE_CORRUPT
        ),
        rusqlite::Error::SqliteFailure(code, _)
        | rusqlite::Error::SqlInputError { error: code, .. }
            if code.extended_code & 0xff == rusqlite::ffi::SQLITE_ERROR =>
        {
            anyhow::anyhow!(
                "{}: retention catalog: {error}",
                super::PRODUCER_STATE_CORRUPT
            )
        }
        _ => error.into(),
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

/// Whether a present retained descriptor still passes check 1 (size, digest, closed grammar,
/// format and decoder version). Its bytes never change, so a failure is permanent: a
/// descriptor written before a grammar change (#800 renamed `language`) can never be
/// admitted again and counts as a missing input (#811). I/O errors stay errors.
pub(crate) fn descriptor_admissible(
    options: &GeneratedImportOptions,
    limits: &crate::semantic_batch::BatchLimits,
) -> Result<bool> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&options.descriptor)?
        .take(limits.descriptor_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(
        crate::semantic_batch::descriptor::Descriptor::parse_admitted(
            &bytes,
            &options.descriptor_sha256,
            limits,
        )
        .is_ok(),
    )
}

pub const FILE: &str = "retention.sqlite";
const SCHEMA: &str = "CREATE TABLE identity(root_key TEXT NOT NULL); CREATE TABLE retained_runs(producer TEXT PRIMARY KEY,languages TEXT NOT NULL,options TEXT NOT NULL,descriptor_sha256 TEXT NOT NULL,source_revision TEXT NOT NULL,attempted_generation TEXT); CREATE TABLE legacy_languages(language TEXT PRIMARY KEY); PRAGMA user_version=1;";

pub(crate) fn attach(db: &Connection, state: &StateDir, root_key: &str) -> Result<()> {
    attach_inner(db, state, root_key).map_err(|error| {
        // A typed catalog fault already names its area; only SQLite corruption is wrapped.
        if unavailable_catalog(&error)
            && !error.to_string().starts_with(super::PRODUCER_STATE_CORRUPT)
        {
            anyhow::anyhow!(
                "{}: retention catalog: {error}",
                super::PRODUCER_STATE_CORRUPT
            )
        } else {
            error
        }
    })
}

#[cfg(test)]
type ProvisionHook = Box<dyn FnOnce(&std::path::Path)>;
#[cfg(test)]
thread_local! {
    static PROVISION_HOOK: std::cell::RefCell<Option<ProvisionHook>> = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn provision_hook(path: &std::path::Path) {
    if let Some(hook) = PROVISION_HOOK.with(|slot| slot.borrow_mut().take()) {
        hook(path);
    }
}

/// Publish a private empty SQLite leaf without exposing SQLite's default 0644
/// creation mode. The staging descriptor is closed before the public name exists.
fn provision_catalog(state: &StateDir) -> Result<std::path::PathBuf> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = state.sqlite_path(FILE)?;
    if path.try_exists()? {
        return Ok(path);
    }
    let staged = state
        .path()
        .join(format!(".{FILE}.tmp-{}", uuid::Uuid::new_v4()));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staged)?;
    struct StagedLeaf(std::path::PathBuf);
    impl Drop for StagedLeaf {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let cleanup = StagedLeaf(staged.clone());
    // Never close a separate descriptor once this inode is a live SQLite file.
    drop(file);
    match std::fs::hard_link(&staged, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::remove_file(&staged)?;
    drop(cleanup);
    let path = state.sqlite_path(FILE)?;
    #[cfg(test)]
    provision_hook(&path);
    Ok(path)
}

fn attach_inner(db: &Connection, state: &StateDir, root_key: &str) -> Result<()> {
    let path = provision_catalog(state)?;
    // Index connections deliberately lack SQLITE_OPEN_CREATE. ATTACH inherits
    // that flag; the private leaf exists before either SQLite connection opens it.
    {
        let mut catalog = Connection::open(&path)?;
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
            catalog_query(tx.query_row("SELECT root_key FROM identity", [], |row| row.get(0)))?;
        if identity != root_key {
            // A catalog written before #811 also bound the root's `st_dev`, which APFS does
            // not keep across reboots: the same record and inode is the same root. Any other
            // identity is a different root. The catalog is disposable, so its slots are
            // dropped rather than refused forever; nothing of the other root is admitted.
            // These are fixed contract statements too: a missing table or column stays typed
            // corruption, while busy, I/O and full-disk faults stay operational.
            if !same_root_other_device(&identity, root_key) {
                catalog_query(
                    tx.execute_batch("DELETE FROM retained_runs; DELETE FROM legacy_languages;"),
                )?;
            }
            catalog_query(tx.execute("UPDATE identity SET root_key=?1", [root_key]))?;
        }
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
    let mut legacy = catalog_query(db.prepare("SELECT language FROM retained.legacy_languages"))?;
    let rows = catalog_query(legacy.query_map([], |row| row.get::<_, String>(0)))?;
    catalog_query(rows.collect::<rusqlite::Result<Vec<_>>>())?;
    Ok(())
}

/// Whether `stored` is the pre-#811 spelling `{record}:{device}:{inode}` of `key`, which is
/// `{record}:{inode}`: the same root seen with a different `st_dev`.
fn same_root_other_device(stored: &str, key: &str) -> bool {
    let Some((record, inode)) = key.rsplit_once(':') else {
        return false;
    };
    stored
        .strip_prefix(record)
        .and_then(|rest| rest.strip_prefix(':'))
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(device, stored_inode)| {
            !device.is_empty()
                && device.bytes().all(|b| b.is_ascii_digit())
                && stored_inode == inode
        })
}

/// Missing state has no hold; unreadable state is an error and cleanup must defer.
pub(crate) fn held_dirs(state: &StateDir) -> Result<BTreeSet<PathBuf>> {
    let path = state.sqlite_path(FILE)?;
    if !path.try_exists()? {
        return Ok(BTreeSet::new());
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_millis(250))?;
    let mut statement =
        catalog_query(db.prepare("SELECT options FROM retained_runs ORDER BY producer"))?;
    let rows = catalog_query(statement.query_map([], |row| row.get::<_, String>(0)))?;
    let options = catalog_query(rows.collect::<rusqlite::Result<Vec<_>>>())?;
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

/// The languages a producer covered in this checkout's durable producer state (#664): a
/// retained run, a successful `producer run` record or a configured pickup. Only these can
/// have had semantic evidence lost to an upgrade. `None` when any run record or the pickup
/// configuration could not be read: a failed read is not evidence that no producer ran, so
/// the caller keeps the recovery history it has.
pub(crate) fn configured_languages(
    state: &StateDir,
    slots: &[RetainedRun],
) -> Option<BTreeSet<String>> {
    use super::state::RunState;
    let mut languages: BTreeSet<String> = slots
        .iter()
        .flat_map(|slot| slot.languages.iter().cloned())
        .collect();
    let runs = state.complete_runs().ok()??;
    let done = runs
        .iter()
        .filter(|run| run.state == RunState::Done)
        .map(|run| run.producer.clone());
    let pickup = state
        .read_pickup_config()
        .ok()?
        .map(|config| config.entries)
        .unwrap_or_default();
    for producer in done.chain(pickup.into_iter().map(|entry| entry.producer)) {
        if let Some(run_languages) = crate::scip_admission::run_languages(&producer) {
            languages.extend(run_languages.iter().map(|language| (*language).to_owned()));
        }
    }
    Some(languages)
}

pub(crate) struct RetainedRun {
    pub producer: String,
    pub languages: Vec<String>,
    pub options: GeneratedImportOptions,
    pub attempted_generation: Option<String>,
}

pub(crate) fn slots(db: &Connection) -> Result<Vec<RetainedRun>> {
    let mut statement = catalog_query(db.prepare("SELECT producer,languages,options,attempted_generation,descriptor_sha256,source_revision FROM retained.retained_runs ORDER BY producer"))?;
    let rows = catalog_query(statement.query_map([], |row| {
        let _: String = row.get(4)?;
        let _: String = row.get(5)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    }))?;
    let rows = catalog_query(rows.collect::<rusqlite::Result<Vec<_>>>())?;
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
        "retained semantic input needs re-import; normal validation is required".into()
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
    fn concurrent_native_publishers_never_observe_nonprivate_catalog_creation() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        use std::sync::{Arc, mpsc};
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.js"), "function a() {}\n").unwrap();
        // Observe the effective mask using an unrelated private test probe, never umask().
        let probe = temp.path().join("mode-probe");
        drop(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o777)
                .open(&probe)
                .unwrap(),
        );
        let probe_mode = fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
        let store = crate::store::Store::open_for_tests(temp.path(), &root).unwrap();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bundle = Arc::new(
            crate::indexer::index_workspace_bundle(
                &crate::indexer::IndexOptions::new(root),
                store.root_id(),
                &cancel,
                |_| {},
            )
            .unwrap(),
        );
        let session = store.writer_session().unwrap();
        let baseline = store.index_baseline().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let a_store = store.clone();
        let a_session = session.clone();
        let a_bundle = bundle.clone();
        let a_cancel = cancel.clone();
        let publisher = std::thread::spawn(move || {
            let hook_ready = ready_tx.clone();
            PROVISION_HOOK.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |path| {
                    let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
                    hook_ready.send(Some(mode)).unwrap();
                    release_rx.recv().unwrap();
                }))
            });
            let result = a_store.publish_native(
                &a_bundle.0,
                &a_bundle.2,
                &a_bundle.1,
                a_session.writer_guard().unwrap(),
                baseline,
                &a_cancel,
            );
            let _ = ready_tx.send(None);
            result
        });
        let observed = ready_rx
            .recv()
            .unwrap()
            .expect("publisher reached provisioning hook");
        let other = store.publish_native(
            &bundle.0,
            &bundle.2,
            &bundle.1,
            session.writer_guard().unwrap(),
            baseline,
            &cancel,
        );
        release_tx.send(()).unwrap();
        let first = publisher.join().unwrap();
        eprintln!(
            "effective test umask={:03o}, visible catalog mode={observed:03o}, first={first:?}, second={other:?}",
            0o777 & !probe_mode
        );
        assert_eq!(usize::from(first.is_ok()) + usize::from(other.is_ok()), 1);
        let loser = first
            .as_ref()
            .err()
            .or_else(|| other.as_ref().err())
            .unwrap();
        assert!(
            loser.to_string().starts_with("revision conflict")
                || crate::store::nonterminal_storage_busy(loser),
            "unexpected CAS loser: {loser:#}"
        );
        assert_eq!(
            observed, 0o600,
            "catalog must be private at its first public visibility"
        );
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
    fn catalog_identity(state: &StateDir) -> String {
        Connection::open(state.path().join(FILE))
            .unwrap()
            .query_row("SELECT root_key FROM identity", [], |row| row.get(0))
            .unwrap()
    }

    fn set_catalog_identity(state: &StateDir, identity: &str) {
        Connection::open(state.path().join(FILE))
            .unwrap()
            .execute("UPDATE identity SET root_key=?1", [identity])
            .unwrap();
    }

    /// #811: APFS renumbers `st_dev` across reboots. A pre-#811 catalog bound to the same
    /// record and inode under another device is the same root: adopted, slots kept.
    #[test]
    fn catalog_bound_to_another_device_is_adopted_with_its_slots() {
        let (temp, state, record) = world();
        let key = format!("{record}:1110064");
        let id = "1".repeat(32);
        state.create_run_dir(&id).unwrap();
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        insert(&db, &state, &id);
        drop(db);
        set_catalog_identity(&state, &format!("{record}:16777234:1110064"));
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        assert_eq!(slots(&db).unwrap().len(), 1, "same root keeps its slot");
        drop(db);
        assert_eq!(catalog_identity(&state), key, "identity rewritten");
        assert_eq!(
            held_dirs(&state).unwrap(),
            BTreeSet::from([state.run_dir(&id).unwrap()])
        );
    }

    /// Any other identity is a different root at this path. The disposable catalog is reset,
    /// not refused forever, and none of the other root's slots survive.
    #[test]
    fn catalog_bound_to_a_different_root_is_reset() {
        for other in [
            "other-record:1110064".to_owned(),
            "other-record:16777234:1110064".to_owned(),
            "{record}:16777234:999".to_owned(),
            "{record}:999".to_owned(),
            "{record}:x:1110064".to_owned(),
        ] {
            let (temp, state, record) = world();
            let other = other.replace("{record}", &record);
            let key = format!("{record}:1110064");
            let id = "1".repeat(32);
            state.create_run_dir(&id).unwrap();
            let db = Connection::open(temp.path().join("index.db")).unwrap();
            attach(&db, &state, &key).unwrap();
            insert(&db, &state, &id);
            db.execute("INSERT INTO retained.legacy_languages VALUES('rust')", [])
                .unwrap();
            drop(db);
            set_catalog_identity(&state, &other);
            let db = Connection::open(temp.path().join("index.db")).unwrap();
            attach(&db, &state, &key).unwrap();
            assert!(slots(&db).unwrap().is_empty(), "{other}: slots dropped");
            let legacy: i64 = db
                .query_row(
                    "SELECT count(*) FROM retained.legacy_languages",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(legacy, 0, "{other}: legacy rows dropped");
            drop(db);
            assert_eq!(catalog_identity(&state), key, "{other}");
        }
    }

    /// Review of #812: rebinding a mismatched identity must not bypass the structural
    /// classification. A damaged catalog stays typed whichever identity it carries.
    #[test]
    fn rebinding_a_damaged_catalog_stays_typed() {
        for table in ["retained_runs", "legacy_languages"] {
            for other in ["different-root", "{record}:16777234:{inode}"] {
                let (temp, state, record) = world();
                let key = format!("{record}:1110064");
                let other = other
                    .replace("{record}", &record)
                    .replace("{inode}", "1110064");
                let db = Connection::open(temp.path().join("index.db")).unwrap();
                attach(&db, &state, &key).unwrap();
                db.execute("UPDATE retained.identity SET root_key=?1", [&other])
                    .unwrap();
                db.execute_batch(&format!("DROP TABLE retained.{table}"))
                    .unwrap();
                drop(db);
                let db = Connection::open(temp.path().join("index.db")).unwrap();
                let error = attach(&db, &state, &key).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .starts_with(super::super::PRODUCER_STATE_CORRUPT)
                        && unavailable_catalog(&error),
                    "{table} under {other}: {error:#}"
                );
            }
        }
    }

    /// An operational fault while rebinding stays operational, never optional corruption.
    #[test]
    fn rebinding_operational_fault_is_not_catalog_corruption() {
        use std::os::unix::fs::PermissionsExt;
        let (temp, state, record) = world();
        let key = format!("{record}:1110064");
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        db.execute("UPDATE retained.identity SET root_key='different-root'", [])
            .unwrap();
        drop(db);
        let path = state.path().join(FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        let error = attach(&db, &state, &key).unwrap_err();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!unavailable_catalog(&error), "{error:#}");
        assert!(
            matches!(
                error.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(code, _))
                    if code.code == rusqlite::ErrorCode::ReadOnly
            ),
            "{error:#}"
        );
        assert_eq!(
            catalog_identity(&state),
            "different-root",
            "nothing rebound"
        );
    }

    #[test]
    fn structural_catalog_faults_are_typed_and_never_recreated() {
        for damage in [
            "DELETE FROM identity",
            "DROP TABLE identity",
            "DROP TABLE retained_runs",
            "DROP TABLE legacy_languages",
            "ALTER TABLE legacy_languages RENAME COLUMN language TO wrong",
            "UPDATE identity SET root_key=zeroblob(4)",
        ] {
            let (temp, state, key) = world();
            let db = Connection::open(temp.path().join("index.db")).unwrap();
            attach(&db, &state, &key).unwrap();
            db.execute_batch(
                &damage
                    .replace("identity", "retained.identity")
                    .replace("retained_runs", "retained.retained_runs")
                    .replace("legacy_languages", "retained.legacy_languages"),
            )
            .unwrap();
            drop(db);
            let db = Connection::open(temp.path().join("index.db")).unwrap();
            let error = attach(&db, &state, &key).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with(super::super::PRODUCER_STATE_CORRUPT),
                "{damage}: {error:#}"
            );
        }
    }

    #[test]
    fn retention_operational_errors_are_not_optional_catalog_corruption() {
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_LOCKED,
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_CANTOPEN,
        ] {
            let error = catalog_query::<()>(Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            )))
            .unwrap_err();
            assert!(!unavailable_catalog(&error), "{code}: {error:#}");
            assert!(
                matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(actual, _)) if actual.extended_code == code)
            );
        }
    }

    #[test]
    fn terminal_prune_releases_superseded_inputs_only_after_pending_hold_ends() {
        let (temp, state, key) = world();
        let (a, b) = ("a".repeat(32), "b".repeat(32));
        for (n, id) in [&a, &b].into_iter().enumerate() {
            let dir = state.create_run_dir(id).unwrap();
            fs::create_dir(dir.join("batch")).unwrap();
            fs::create_dir(dir.join("out")).unwrap();
            state
                .write_run_record(&super::super::state::RunRecord {
                    version: 1,
                    run_id: id.clone(),
                    producer: "producer".into(),
                    trust_profile: super::super::TrustProfile::Restricted,
                    plan_sha256: "a".repeat(64),
                    source_revision: "native".into(),
                    state: super::super::state::RunState::Done,
                    outcome: Some("imported".into()),
                    error_code: None,
                    daemon: super::super::state::DaemonIdentity::current(),
                    process: None,
                    group_status: None,
                    failure: None,
                    origin: super::super::state::RunOrigin::Manual,
                    request_id: None,
                    created_at: n as u64,
                    finished_at: Some(n as u64),
                    diagnostics: None,
                })
                .unwrap();
        }
        let db = Connection::open(temp.path().join("index.db")).unwrap();
        attach(&db, &state, &key).unwrap();
        insert(&db, &state, &b);
        drop(db);
        state
            .prune_runs(&BTreeSet::from([state.run_dir(&a).unwrap()]))
            .unwrap();
        assert!(state.run_dir(&a).unwrap().join("batch").exists());
        state.prune_runs(&BTreeSet::new()).unwrap();
        assert!(!state.run_dir(&a).unwrap().join("batch").exists());
        assert!(!state.run_dir(&a).unwrap().join("out").exists());
        assert!(state.run_dir(&a).unwrap().join("run.json").exists());
        assert!(state.run_dir(&b).unwrap().join("batch").exists());
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
    fn provisioning_refuses_existing_nonprivate_and_symlink_leaves() {
        use std::os::unix::fs::PermissionsExt;
        let (_temp, state, _key) = world();
        let path = state.path().join(FILE);
        fs::write(&path, b"existing").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            provision_catalog(&state)
                .unwrap_err()
                .to_string()
                .starts_with(super::super::UNSAFE_STATE)
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&path).unwrap(), b"existing");
        fs::remove_file(&path).unwrap();
        let target = state.path().join("target");
        fs::write(&target, b"existing").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(
            provision_catalog(&state)
                .unwrap_err()
                .to_string()
                .starts_with(super::super::UNSAFE_STATE)
        );
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&target).unwrap(), b"existing");
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
