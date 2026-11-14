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
fn gc_report_releases_witnesses_for_inactive_checkouts() {
    const CHILD: &str = "TRELLIS_GC_REPORT_WITNESS_CHILD";
    const TEST: &str =
        "store::sqlite_deleted_witness_tests::gc_report_releases_witnesses_for_inactive_checkouts";
    if std::env::var_os(CHILD).is_none() {
        let outcome = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            outcome.status.success(),
            "child output:\n{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        return;
    }

    let state = tempfile::tempdir().unwrap();
    let workspaces = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let roots = topology::TopologyRoots::isolated_for_tests(
        state.path().join("cache"),
        state.path().join("data"),
    );
    let mut candidates = Vec::new();
    for workspace in &workspaces {
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let path = roots.index_db(&store.identity);
        let key = store.identity.root_key.clone();
        store.retire_checkout_sqlite_witnesses();
        drop(store);
        let metadata = std::fs::metadata(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        candidates.push((key, path, (metadata.dev(), metadata.ino()), bytes));
    }
    assert_ne!(candidates[0].2, candidates[1].2);

    // Examine only our own paths, including live registrations and exact inodes.
    let snapshot = || {
        let registry = sqlite_witnesses().lock().unwrap();
        candidates
            .iter()
            .map(|(_, path, _, _)| {
                let inodes = registry
                    .by_path
                    .get(path)
                    .into_iter()
                    .flatten()
                    .map(|file| {
                        let metadata = file.metadata().unwrap();
                        (metadata.dev(), metadata.ino())
                    })
                    .collect::<Vec<_>>();
                let live = registry
                    .live
                    .iter()
                    .filter(|((name, _, _), _)| name == path)
                    .map(|(_, count)| count)
                    .sum::<usize>();
                (path.clone(), inodes, live)
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot();
    assert!(
        before
            .iter()
            .all(|(_, inodes, live)| inodes.is_empty() && *live == 0),
        "setup retirement: {before:?}"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 31 * 86_400;
    let report = || {
        let derived = roots.gc_report_at(now).unwrap().derived;
        assert_eq!(derived.len(), candidates.len());
        for (key, path, inode, bytes) in &candidates {
            let entry = derived.iter().find(|entry| &entry.root_key == key).unwrap();
            assert_eq!((entry.status, entry.reason), ("eligible", "age_30_days"));
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!((metadata.dev(), metadata.ino()), *inode);
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
        }
    };
    report();
    let first = snapshot();
    report();
    let second = snapshot();
    for (i, (_, inodes, live)) in second.iter().enumerate() {
        assert_eq!(*live, 0);
        assert!(
            inodes.len() <= first[i].1.len(),
            "same inode grew: {first:?} -> {second:?}"
        );
        assert!(inodes.len() <= 1 && inodes.iter().all(|inode| *inode == candidates[i].2));
    }
    eprintln!("report witness trace: before={before:?}, first={first:?}, second={second:?}");
    assert!(
        second.iter().all(|(_, inodes, _)| inodes.is_empty()),
        "quiescent GC reports retained candidate witnesses: {second:?}"
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
            &crate::model::IndexOptions::new(workspace.path().to_owned()),
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
                &crate::model::IndexOptions::new(workspace.path().to_owned()),
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
        // A passing child's output is otherwise dropped: relay its cleanup
        // note (#1029) with a write that cannot panic.
        for line in String::from_utf8_lossy(&outcome.stderr).lines() {
            if line.starts_with(IMAGE_NOT_EJECTED) {
                use std::io::Write as _;
                let _ = writeln!(std::io::stderr(), "{line}");
            }
        }
        return;
    }
    use std::os::fd::AsRawFd;
    use std::process::Command;
    const IMAGE_NOT_EJECTED: &str = "note: isolated filesystem test image was not ejected";
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
        // No image verification and no automatic fsck: neither is needed for
        // a fresh image, and fsck opens the device node, which blocks eject.
        .args([
            "attach",
            "-nobrowse",
            "-noverify",
            "-noautofsck",
            "-mountpoint",
        ])
        .arg(&mount)
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        attached.status.success(),
        "hdiutil attach: {}",
        String::from_utf8_lossy(&attached.stderr)
    );
    // Attach output starts with the whole device, before its partitions.
    // Keep that identity: a failed eject may already have unmounted the volume.
    let attach_output = String::from_utf8(attached.stdout).unwrap();
    let device = attach_output.split_whitespace().next().unwrap();
    assert!(
        device.starts_with("/dev/disk"),
        "hdiutil attach: {attach_output}"
    );
    struct Mounted {
        device: std::path::PathBuf,
        attached: bool,
    }
    impl Mounted {
        // Anything holding the device node open keeps the image busy after
        // its volume is unmounted, and `-force` does not override that
        // (#131, #1029). Retry with backoff, forcing after the first
        // attempts, and keep every attempt's message. The bound is a hang
        // backstop, not an expectation of how long an eject takes.
        fn detach(&mut self) -> Result<()> {
            let started = std::time::Instant::now();
            let bound = std::time::Duration::from_secs(30);
            let mut delay = std::time::Duration::from_millis(250);
            let mut failures = Vec::new();
            for attempt in 0.. {
                let mut command = Command::new("/usr/bin/hdiutil");
                command.arg("detach").arg(&self.device);
                if attempt >= 3 {
                    command.arg("-force");
                }
                let outcome = command.output()?;
                if outcome.status.success() {
                    self.attached = false;
                    return Ok(());
                }
                failures.push(String::from_utf8_lossy(&outcome.stderr).trim().to_owned());
                if started.elapsed() >= bound {
                    break;
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(std::time::Duration::from_secs(2));
            }
            anyhow::bail!("hdiutil detach: {failures:?}")
        }
    }
    impl Drop for Mounted {
        fn drop(&mut self) {
            if self.attached
                && let Err(error) = self.detach()
            {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "isolated filesystem cleanup failed: {error:#}"
                );
            }
        }
    }
    let mut mounted = Mounted {
        device: device.into(),
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
    // A detach can unmount the volume before failing to eject the image.
    // Exercise that state: cleanup must still address the attached device.
    let unmounted = Command::new("/usr/bin/hdiutil")
        .arg("unmount")
        .arg(&mount)
        .output()
        .unwrap();
    assert!(
        unmounted.status.success(),
        "hdiutil unmount: {}",
        String::from_utf8_lossy(&unmounted.stderr)
    );
    let ejected = mounted.detach();
    // The volume staying mounted is a real cleanup failure, whatever the
    // eject said.
    assert_ne!(
        std::fs::metadata(&mount).unwrap().dev(),
        mounted_device,
        "test image remained mounted after detach: {ejected:?}"
    );
    // An image that is unmounted but still busy after the bound is a host
    // condition, not a product defect (#1029): report it and pass. The image
    // file is still removed with its directory; the device keeps the unlinked
    // file until the host ejects it or restarts, and nothing stays mounted.
    if let Err(error) = ejected {
        mounted.attached = false;
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stderr(),
            "{IMAGE_NOT_EJECTED}: {} is unmounted but still attached: {error:#}",
            mounted.device.display()
        );
    }
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
