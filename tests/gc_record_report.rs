use crate::common;
use std::os::unix::fs::MetadataExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use trellis::{
    model::Annotation,
    store::topology::{DurableRecords, UseGuard, WorkspaceIdentity},
};

// This isolated child leaves a real SQLite rollback journal after an uncommitted write.
#[test]
fn leave_hot_journal_child() {
    let Some(path) = std::env::var_os("GC_HOT_RECORD_DB") else {
        return;
    };
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch("BEGIN IMMEDIATE; INSERT INTO views(id,payload) VALUES('hot','uncommitted');")
        .unwrap();
    std::process::exit(0);
}

fn annotation(roots: &trellis::store::topology::TopologyRoots, identity: &WorkspaceIdentity) {
    DurableRecords::new(roots, identity)
        .put_annotation(&Annotation {
            id: "note".into(),
            node_id: "node".into(),
            body: "saved".into(),
        })
        .unwrap();
}

type Snapshot = Vec<(PathBuf, Vec<u8>, u64, i64, i64)>;
fn snapshot(root: &Path) -> Snapshot {
    fn walk(root: &Path, path: &Path, out: &mut Snapshot) {
        let mut entries: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let m = fs::symlink_metadata(&path).unwrap();
            let bytes = if m.is_file() {
                fs::read(&path).unwrap()
            } else {
                vec![]
            };
            out.push((
                path.strip_prefix(root).unwrap().to_owned(),
                bytes,
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
            ));
            if m.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = vec![];
    walk(root, root, &mut out);
    out
}

#[test]
fn report_continues_past_busy_and_hot_journal_records_without_writes() {
    let (temp, roots) = common::fixture();
    let mut identities: Vec<_> = (0..3)
        .map(|i| {
            let work = temp.path().join(format!("work-{i}"));
            fs::create_dir(&work).unwrap();
            WorkspaceIdentity::discover(Some(&work), &work).unwrap()
        })
        .collect();
    identities.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    for identity in &identities {
        annotation(&roots, identity);
    }
    let busy = &identities[0];
    let hot = &identities[1];
    let valid = &identities[2];
    let db_path = roots.record_db(hot);
    let result = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("gc_record_report::leave_hot_journal_child")
        .env("GC_HOT_RECORD_DB", &db_path)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let journal = db_path.with_file_name("workspace.db-journal");
    assert!(fs::metadata(&journal).unwrap().len() > 512);
    // A hot journal left by a crashed writer is recovery, not contention, and is not rolled back.
    let journal_bytes = fs::read(&journal).unwrap();
    let error = DurableRecords::new(&roots, hot).views().unwrap_err();
    assert_eq!(
        error.to_string(),
        "incomplete_record: recovery required",
        "{error:#}"
    );
    assert_eq!(fs::read(&journal).unwrap(), journal_bytes);
    let held = UseGuard::acquire_existing(&roots.record_use_lock(busy), false, true).unwrap();
    let before = snapshot(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(
        before,
        snapshot(temp.path()),
        "report must preserve bytes and mtimes"
    );
    assert_eq!(
        report
            .records
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        identities
            .iter()
            .map(|i| i.record_id.as_str())
            .collect::<Vec<_>>()
    );
    let value = serde_json::to_value(&report).unwrap();
    let records = value["records"].as_array().unwrap();
    assert_eq!(
        records[0],
        serde_json::json!({"id":busy.record_id,"status":"busy","reason":"use_lock_busy"})
    );
    assert_eq!(
        records[1],
        serde_json::json!({"id":hot.record_id,"status":"unknown","reason":"recovery_sidecar"})
    );
    assert_eq!(records[2]["id"], valid.record_id);
    assert_eq!(records[2]["status"], "valid");
    assert_eq!(records[2]["reason"], "record_readable");
    assert_eq!(records[2]["views"], 0);
    assert_eq!(records[2]["annotations"], 1);
    assert_eq!(records[2]["missingKnownPaths"], serde_json::json!([]));
    assert!(roots.record_by_id(&hot.record_id).is_err());
    assert!(
        roots
            .forget_with_confirmation(&hot.record_id, |_, _| panic!(
                "unsafe record cannot be confirmed"
            ))
            .is_err()
    );
    assert!(journal.exists(), "forget must not delete recovery sidecars");
    drop(held);
}

#[test]
fn incomplete_record_does_not_hide_valid_peer_or_create_missing_lock() {
    let (temp, roots) = common::fixture();
    let work = temp.path().join("valid");
    let incomplete_work = temp.path().join("incomplete");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&incomplete_work).unwrap();
    let valid = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let incomplete = WorkspaceIdentity::discover(Some(&incomplete_work), &incomplete_work).unwrap();
    annotation(&roots, &valid);
    roots.prepare_records(&incomplete).unwrap();
    common::private(&roots.record_dir(&incomplete));
    let lock = roots.record_use_lock(&incomplete);
    // An existing verified lock with no DB is an incomplete record, not an omitted row.
    drop(UseGuard::acquire(&lock, false, false).unwrap());
    let before = snapshot(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(before, snapshot(temp.path()));
    let rows = serde_json::to_value(&report).unwrap()["records"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), 2);
    let invalid = rows
        .iter()
        .find(|r| r["id"] == incomplete.record_id)
        .unwrap();
    assert_eq!(
        invalid,
        &serde_json::json!({"id":incomplete.record_id,"status":"unknown","reason":"incomplete_record"})
    );
    assert_eq!(
        rows.iter().find(|r| r["id"] == valid.record_id).unwrap()["status"],
        "valid"
    );
    fs::remove_file(lock).unwrap();
    let before = snapshot(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(before, snapshot(temp.path()));
    assert!(!roots.record_use_lock(&incomplete).exists());
    assert_eq!(
        report
            .records
            .iter()
            .find(|r| r.id == incomplete.record_id)
            .unwrap()
            .reason,
        "unsafe_use_lock"
    );
}

#[test]
fn hostile_path_text_and_non_utf8_entries_cannot_spoof_status_or_abort_inventory() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, os::unix::fs::PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let hostile = temp
        .path()
        .join("storage_busy recovery sidecar present incomplete_record");
    common::private(&hostile);
    let roots = trellis::store::topology::TopologyRoots::isolated_for_tests(
        hostile.join("cache"),
        hostile.join("data"),
    );
    let mut ids: Vec<_> = (0..2)
        .map(|n| {
            let work = temp.path().join(format!("workspace-{n}"));
            fs::create_dir(&work).unwrap();
            WorkspaceIdentity::discover(Some(&work), &work).unwrap()
        })
        .collect();
    ids.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    for id in &ids {
        annotation(&roots, id);
    }
    roots.prepare_index(&ids[0]).unwrap();
    for directory in [roots.cache.join("indexes"), roots.data.join("workspaces")] {
        let raw_name = directory.join(OsString::from_vec(b"not-a-managed-id-\xff".to_vec()));
        if let Err(error) = fs::write(raw_name, b"unrelated") {
            // macOS/APFS refuses non-UTF8 names at creation (EILSEQ); Linux
            // accepts the raw entry and exercises report enumeration below.
            assert!(
                cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EILSEQ),
                "{error}"
            );
        }
    }
    let unsafe_lock = roots.record_use_lock(&ids[0]);
    fs::set_permissions(&unsafe_lock, fs::Permissions::from_mode(0o644)).unwrap();
    let before = snapshot(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(before, snapshot(temp.path()));
    assert_eq!(report.records.len(), 2);
    assert_eq!(
        (report.records[0].status, report.records[0].reason),
        ("unknown", "unsafe_use_lock")
    );
    assert_eq!(report.records[1].status, "valid");
    fs::set_permissions(&unsafe_lock, fs::Permissions::from_mode(0o600)).unwrap();
    let unsafe_db = roots.record_db(&ids[0]);
    fs::set_permissions(&unsafe_db, fs::Permissions::from_mode(0o644)).unwrap();
    let before = snapshot(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(before, snapshot(temp.path()));
    assert_eq!(report.records.len(), 2);
    assert_eq!(
        (report.records[0].status, report.records[0].reason),
        ("unknown", "metadata_unreadable")
    );
    assert_eq!(report.records[1].status, "valid");
    assert!(
        roots.record_by_id(&ids[0].record_id).is_err(),
        "forget inventory must remain strict"
    );
}

#[test]
fn symlink_record_directory_never_probes_outside_with_or_without_database() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let (temp, roots) = common::fixture();
    let work = temp.path().join("valid-work");
    let linked_work = temp.path().join("linked-work");
    fs::create_dir(&work).unwrap();
    fs::create_dir(&linked_work).unwrap();
    let valid = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let linked = WorkspaceIdentity::discover(Some(&linked_work), &linked_work).unwrap();
    annotation(&roots, &valid);
    let outside = temp.path().join("outside-private");
    common::private(&outside);
    symlink(&outside, roots.record_dir(&linked)).unwrap();
    drop(UseGuard::acquire(&roots.record_use_lock(&linked), false, false).unwrap());
    let assert_report = || {
        let before = snapshot(&roots.data);
        let report = roots.gc_report_at(1_800_000_000).unwrap();
        assert_eq!(before, snapshot(&roots.data));
        assert_eq!(report.records.len(), 2);
        let unsafe_row = report
            .records
            .iter()
            .find(|r| r.id == linked.record_id)
            .unwrap();
        assert_eq!(
            (unsafe_row.status, unsafe_row.reason),
            ("unknown", "unsafe_record_directory")
        );
        assert!(unsafe_row.views.is_none());
        assert_eq!(
            report
                .records
                .iter()
                .find(|r| r.id == valid.record_id)
                .unwrap()
                .status,
            "valid"
        );
        assert!(roots.record_by_id(&linked.record_id).is_err());
    };
    assert_report();
    assert!(fs::read_dir(&outside).unwrap().next().is_none());

    // An unreadable outside DB would expose any attempt to open the symlink target.
    let outside_db = outside.join("workspace.db");
    fs::write(&outside_db, b"do not read outside managed tree").unwrap();
    fs::set_permissions(&outside_db, fs::Permissions::from_mode(0o000)).unwrap();
    let before = fs::metadata(&outside_db).unwrap();
    assert_report();
    let after = fs::metadata(&outside_db).unwrap();
    assert_eq!(
        (
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.atime(),
            before.atime_nsec()
        ),
        (
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.atime(),
            after.atime_nsec()
        )
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    fs::set_permissions(&outside_db, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        fs::read(&outside_db).unwrap(),
        b"do not read outside managed tree"
    );
}
