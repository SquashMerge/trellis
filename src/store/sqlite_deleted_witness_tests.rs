use super::*;
use std::os::unix::fs::MetadataExt;

#[test]
fn fixed_capacity_fails_before_opening_another_sqlite_inode() {
    if std::env::var_os("TRELLIS_WITNESS_CAP_CHILD").is_none() {
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::sqlite_deleted_witness_tests::fixed_capacity_fails_before_opening_another_sqlite_inode")
                .env("TRELLIS_WITNESS_CAP_CHILD", "1")
                .output().unwrap();
        assert!(
            outcome.status.success(),
            "{}",
            String::from_utf8_lossy(&outcome.stdout)
        );
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let path = state.path().join("index.db");
    {
        let mut registry = sqlite_witnesses().lock().unwrap();
        registry.count = MAX_RETAINED_SQLITE_WITNESSES;
    }
    let error = retained_sqlite_file(&path, true, true, false).unwrap_err();
    assert!(error.to_string().contains("capacity reached"));
    assert!(
        !path.exists(),
        "capacity refusal must precede file creation"
    );
}

#[test]
fn idle_retirement_defers_sqlite_witness_until_last_managed_connection_closes() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let path = store.roots.index_db(&store.identity);
    let live = open_index(&path, false).unwrap();
    store.retire_checkout_sqlite_witnesses();
    assert!(
        sqlite_witnesses()
            .lock()
            .unwrap()
            .by_path
            .contains_key(&path),
        "closing a check FD while SQLite owns an inode drops its process locks"
    );
    drop(live);
    assert!(
        !sqlite_witnesses()
            .lock()
            .unwrap()
            .by_path
            .contains_key(&path),
        "last protected connection completes deferred release"
    );
}

#[test]
fn exclusive_release_closes_only_deleted_index_after_all_sqlite_connections() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let path = store.roots.index_db(&store.identity);
    let guard = store
        .roots
        .index_use_exclusive_existing(&store.identity)
        .unwrap();
    let db = open_index(&path, false).unwrap();
    let inode = std::fs::metadata(&path).unwrap();
    let inode = (inode.dev(), inode.ino());
    assert!(
        release_deleted_sqlite_witness(&path, inode, &guard).is_err(),
        "a named live index cannot be released"
    );
    std::fs::remove_file(&path).unwrap();
    assert!(
        release_deleted_sqlite_witness(&path, inode, &guard).is_err(),
        "a deleted inode with an open SQLite connection must remain witnessed"
    );
    drop(db);
    release_deleted_sqlite_witness(&path, inode, &guard).unwrap();
    let registry = sqlite_witnesses().lock().unwrap();
    assert!(
        !registry.by_path.contains_key(&path),
        "deleted file descriptor must be closed"
    );
}
#[test]
fn aborted_stage_retries_and_successful_recreations_do_not_accumulate_witnesses() {
    if std::env::var_os("TRELLIS_STAGE_RETRY_CHILD").is_none() {
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::sqlite_deleted_witness_tests::aborted_stage_retries_and_successful_recreations_do_not_accumulate_witnesses")
                .env("TRELLIS_STAGE_RETRY_CHILD", "1")
                .output().unwrap();
        assert!(
            outcome.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
    for _ in 0..8 {
        let error =
            Store::open_for_tests_with_index_stage_hook(state.path(), workspace.path(), |_| {
                anyhow::bail!("injected aborted stage")
            })
            .unwrap_err();
        assert!(error.to_string().contains("injected aborted stage"));
        let registry = sqlite_witnesses().lock().unwrap();
        assert_eq!(
            registry.count, 0,
            "aborted stage leaked an inode descriptor/cap slot"
        );
        assert!(registry.by_path.is_empty());
    }
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    // Exceptional recreation requires a durable queue file. Seed it before
    // damaging the index; the test still measures only index witnesses.
    let accepted = store
        .enqueue_request(
            &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
            None,
        )
        .unwrap();
    assert_eq!(accepted.state, "queued");
    let path = store.roots.index_db(&store.identity);
    let queue = store.request_db_path();
    {
        let registry = sqlite_witnesses().lock().unwrap();
        assert_eq!(
            registry.count, 2,
            "index and accepted queue each retain one witness"
        );
        assert_eq!(registry.by_path.get(&path).map(Vec::len), Some(1));
        assert_eq!(registry.by_path.get(&queue).map(Vec::len), Some(1));
    }
    drop(store);
    for _ in 0..3 {
        std::fs::write(&path, b"short").unwrap();
        let recovering = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (_, leader) = recovering
            .recreate_pending_writer_session(
                &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        drop(leader);
        let registry = sqlite_witnesses().lock().unwrap();
        assert_eq!(
            registry.count, 2,
            "recreation must retain only the active index and durable queue witnesses"
        );
        assert_eq!(registry.by_path.get(&path).map(Vec::len), Some(1));
        assert_eq!(registry.by_path.get(&queue).map(Vec::len), Some(1));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn isolated_filesystem_returns_allocated_blocks_only_after_verified_release() {
    if std::env::var_os("TRELLIS_SPACE_CHILD").is_none() {
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::sqlite_deleted_witness_tests::isolated_filesystem_returns_allocated_blocks_only_after_verified_release")
                .env("TRELLIS_SPACE_CHILD", "1")
                .output().unwrap();
        assert!(
            outcome.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        return;
    }
    use std::os::fd::AsRawFd;
    use std::process::Command;
    let home = tempfile::tempdir().unwrap();
    let image = home.path().join("witness.sparseimage");
    let mount = home.path().join("mounted");
    std::fs::create_dir(&mount).unwrap();
    let created = Command::new("/usr/bin/hdiutil")
        .args([
            "create",
            "-size",
            "128m",
            "-fs",
            "HFS+",
            "-volname",
            "trellis-witness-test",
            "-type",
            "SPARSE",
            "-quiet",
        ])
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "hdiutil create: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    let attached = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-nobrowse", "-mountpoint"])
        .arg(&mount)
        .arg("-quiet")
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        attached.status.success(),
        "hdiutil attach: {}",
        String::from_utf8_lossy(&attached.stderr)
    );
    struct Mounted {
        path: std::path::PathBuf,
        attached: bool,
    }
    impl Mounted {
        // A system daemon can briefly hold a fresh volume busy (#131):
        // retry, then force, keeping every attempt's message.
        fn detach(&mut self) -> Result<()> {
            let mut failures = Vec::new();
            for attempt in 0..4 {
                let mut command = Command::new("/usr/bin/hdiutil");
                command.arg("detach").arg(&self.path);
                if attempt == 3 {
                    command.arg("-force");
                }
                let outcome = command.output()?;
                if outcome.status.success() {
                    self.attached = false;
                    return Ok(());
                }
                failures.push(String::from_utf8_lossy(&outcome.stderr).trim().to_owned());
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            anyhow::bail!("hdiutil detach: {failures:?}")
        }
    }
    impl Drop for Mounted {
        fn drop(&mut self) {
            if self.attached
                && let Err(error) = self.detach()
            {
                eprintln!("isolated filesystem cleanup failed: {error:#}");
            }
        }
    }
    let mut mounted = Mounted {
        path: mount.clone(),
        attached: true,
    };
    let mounted_device = std::fs::metadata(&mount).unwrap().dev();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o700)).unwrap();
    fn available(path: &Path) -> u64 {
        use std::ffi::CString;
        let cpath = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) }, 0);
        u64::from(stat.f_bavail) * stat.f_frsize
    }
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(&mount, workspace.path()).unwrap();
    let path = store.roots.index_db(&store.identity);
    let db = protected_sqlite_open(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    db.execute_batch("CREATE TABLE witness_allocation(payload BLOB); INSERT INTO witness_allocation VALUES(zeroblob(16777216));").unwrap();
    drop(db);
    let named = std::fs::metadata(&path).unwrap();
    let allocated = named.blocks() * 512;
    assert!(
        allocated >= 8 * 1024 * 1024,
        "SQLite payload is sparse: {allocated}"
    );
    let inode = (named.dev(), named.ino());
    let descriptor = {
        let registry = sqlite_witnesses().lock().unwrap();
        let file = registry.by_path.get(&path).unwrap().first().unwrap();
        file.as_raw_fd()
    };
    let guard = store
        .roots
        .index_use_exclusive_existing(&store.identity)
        .unwrap();
    let before = available(&mount);
    std::fs::remove_file(&path).unwrap();
    let still_held = available(&mount);
    assert!(
        still_held <= before + allocated / 4,
        "deleted inode blocks returned before fd close: before={before}, held={still_held}, allocated={allocated}"
    );
    assert!(
        unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0,
        "retained deleted-inode fd was already closed"
    );
    release_deleted_sqlite_witness(&path, inode, &guard).unwrap();
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_GETFD) },
        -1,
        "the exact deleted-inode kernel fd survived release"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
    let after = available(&mount);
    assert!(
        after >= still_held + allocated / 2,
        "filesystem did not reclaim allocated deleted-inode blocks: held={still_held}, after={after}, allocated={allocated}"
    );
    drop(guard);
    drop(store);
    mounted.detach().unwrap();
    assert_ne!(
        std::fs::metadata(&mount).unwrap().dev(),
        mounted_device,
        "test image remained mounted after successful detach"
    );
}
#[test]
fn foreign_replacement_stage_refuses_release_and_preserves_foreign_inode() {
    if std::env::var_os("TRELLIS_FOREIGN_STAGE_CHILD").is_none() {
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("store::sqlite_deleted_witness_tests::foreign_replacement_stage_refuses_release_and_preserves_foreign_inode")
                .env("TRELLIS_FOREIGN_STAGE_CHILD", "1")
                .output().unwrap();
        assert!(
            outcome.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        return;
    }
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let exclusive = store
        .roots
        .index_use_exclusive_existing(&store.identity)
        .unwrap();
    let leader = store
        .roots
        .writer_under_exclusive(store.identity.clone(), exclusive)
        .unwrap();
    let stage = store.create_staged_index(&leader, true).unwrap();
    let path = stage.path.clone();
    let held = stage.file.metadata().unwrap();
    let obsolete = (held.dev(), held.ino());
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"foreign stage cannot be removed").unwrap();
    let foreign = std::fs::symlink_metadata(&path).unwrap();
    assert_ne!((foreign.dev(), foreign.ino()), obsolete);
    let error = store
        .cleanup_failed_staged_index(stage, Some(&leader))
        .unwrap_err();
    assert!(error.is::<ForeignStagedIndex>(), "wrong failure: {error:#}");
    let after = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!((foreign.dev(), foreign.ino()), (after.dev(), after.ino()));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"foreign stage cannot be removed"
    );
    let guard = leader
        .exclusive_use_guard(&store.roots.index_use_lock(&store.identity))
        .unwrap();
    let error = release_deleted_sqlite_witness_kind(&path, obsolete, guard, true).unwrap_err();
    assert!(
        error.is::<ForeignStagedIndex>(),
        "direct release did not fail closed: {error:#}"
    );
    let registry = sqlite_witnesses().lock().unwrap();
    let files = registry.by_path.get(&path).unwrap();
    assert_eq!(files.len(), 1, "obsolete witness must remain cache-owned");
    let retained = files[0].metadata().unwrap();
    assert_eq!((retained.dev(), retained.ino()), obsolete);
    assert_eq!(Arc::strong_count(&files[0]), 1);
    assert_eq!(registry.live.get(&(path, obsolete.0, obsolete.1)), None);
}
