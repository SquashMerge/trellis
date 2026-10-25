mod common;
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
};
use trellis::store::topology::{UseGuard, WorkspaceIdentity};

fn root(base: &Path) -> std::path::PathBuf {
    let work = base.join("work");
    fs::create_dir(&work).unwrap();
    work
}

#[test]
fn discovery_matrix() {
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    fs::create_dir(work.join("nested")).unwrap();
    let plain = WorkspaceIdentity::discover(None, &work.join("nested")).unwrap();
    assert_eq!(plain.root, fs::canonicalize(work.join("nested")).unwrap());
    assert!(plain.record_id.starts_with("path-"));
    common::private(&work.join(".git"));
    let git = WorkspaceIdentity::discover(None, &work.join("nested")).unwrap();
    assert_eq!(git.root, fs::canonicalize(&work).unwrap());
    assert_eq!(git.record_id.len(), 36);
    assert_ne!(git.record_id, plain.record_id);
    git.verify().unwrap();
    let other = WorkspaceIdentity::discover(Some(&work.join("nested")), &work).unwrap();
    assert_eq!(other.root, fs::canonicalize(work.join("nested")).unwrap());
    let old = fs::rename(&work, temp.path().join("moved"));
    old.unwrap();
    assert!(
        git.verify()
            .unwrap_err()
            .to_string()
            .contains("root_changed")
    );
}

#[test]
fn implicit_home_is_refused_in_isolated_child() {
    let (temp, _) = common::fixture();
    let home = fs::canonicalize(root(temp.path())).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("implicit_home_child")
        .arg("--nocapture")
        .env("HOME", &home)
        .env("TOPOLOGY_IMPLICIT_HOME_CHILD", &home)
        .current_dir(&home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated home check failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn implicit_home_child() {
    let Some(home) = std::env::var_os("TOPOLOGY_IMPLICIT_HOME_CHILD") else {
        return;
    };
    let home = Path::new(&home);
    assert_eq!(
        std::env::current_dir().unwrap(),
        fs::canonicalize(home).unwrap()
    );
    assert_eq!(std::env::var_os("HOME").as_deref(), Some(home.as_os_str()));
    let error = WorkspaceIdentity::discover(None, home).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("implicit home or filesystem root refused"),
        "unexpected home refusal: {error}"
    );
    // The same location is accepted when the workspace is explicitly selected.
    assert_eq!(
        WorkspaceIdentity::discover(Some(home), home).unwrap().root,
        home
    );
}

#[test]
fn implicit_filesystem_root_is_refused() {
    let filesystem_root = Path::new("/");
    let error = WorkspaceIdentity::discover(None, filesystem_root).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("implicit home or filesystem root refused"),
        "unexpected filesystem-root refusal: {error}"
    );
}

#[test]
fn path_identity_is_sha256_of_canonical_absolute_root_bytes() {
    use sha2::{Digest, Sha256};
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    let nested = work.join("nested");
    fs::create_dir(&nested).unwrap();
    let selected = nested.join("..").join("nested");
    let canonical = fs::canonicalize(&selected).unwrap();
    let expected_key = hex::encode(Sha256::digest(canonical.to_str().unwrap().as_bytes()));
    let identity = WorkspaceIdentity::discover(Some(&selected), &work).unwrap();
    assert!(canonical.is_absolute());
    assert_eq!(identity.root, canonical);
    assert_eq!(identity.root_key, expected_key);
    assert_eq!(identity.record_id, format!("path-{expected_key}"));
    assert_ne!(
        identity.root_key,
        hex::encode(Sha256::digest(selected.to_str().unwrap().as_bytes()))
    );
}

#[test]
fn fixed_paths_and_unsafe_components() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    assert_eq!(
        roots.index_db(&identity),
        roots
            .cache
            .join("indexes")
            .join(&identity.root_key)
            .join("index.db")
    );
    assert_eq!(
        roots.record_db(&identity),
        roots
            .data
            .join("workspaces")
            .join(&identity.record_id)
            .join("workspace.db")
    );
    roots.prepare_index(&identity).unwrap();
    use std::os::unix::fs::symlink;
    fs::remove_dir(roots.index_dir(&identity)).unwrap();
    symlink(&work, roots.index_dir(&identity)).unwrap();
    assert!(roots.index_use(&identity).is_err());
}

#[test]
fn git_marker_matrix() {
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let marker = work.join(".git/trellis/workspace-id");
    assert_eq!(fs::read(&marker).unwrap().len(), 36);
    assert_eq!(
        WorkspaceIdentity::discover(Some(&work), &work)
            .unwrap()
            .record_id,
        identity.record_id
    );
    fs::write(&marker, b"aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa").unwrap();
    assert!(
        identity
            .verify()
            .unwrap_err()
            .to_string()
            .contains("workspace_id_changed")
    );
    fs::write(&marker, b"invalid").unwrap();
    assert!(WorkspaceIdentity::discover(Some(&work), &work).is_err());
    let linked = temp.path().join("linked");
    fs::create_dir(&linked).unwrap();
    fs::write(linked.join(".git"), "gitdir: ../work/.git\n").unwrap();
    // A valid pointer selects the same Git directory, hence the marker must be valid.
    assert!(WorkspaceIdentity::discover(Some(&linked), &linked).is_err());
    fs::write(&marker, identity.record_id.as_bytes()).unwrap();
    let pointed = WorkspaceIdentity::discover(Some(&linked), &linked).unwrap();
    assert_eq!(pointed.record_id, identity.record_id);
    fs::write(linked.join(".git"), "gitdir: ../work/.git\nextra\n").unwrap();
    assert!(WorkspaceIdentity::discover(Some(&linked), &linked).is_err());
}

#[test]
fn marker_race_and_durability() {
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let children: Vec<_> = (0..4)
        .map(|_| {
            Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("marker_child")
                .arg("--nocapture")
                .env("TOPOLOGY_MARKER_CHILD", &work)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut ids = vec![];
    for child in children {
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );
        let output = String::from_utf8_lossy(&result.stdout);
        // With one test thread libtest prints `test marker_child ... ` before
        // the test runs, so the marker can follow it on the same line (#184).
        ids.push(
            output
                .lines()
                .find_map(|line| line.split_once("MARKER_ID=").map(|(_, id)| id))
                .unwrap_or_else(|| panic!("child printed no marker ID: {output}"))
                .to_owned(),
        );
    }
    assert!(ids.iter().all(|id| id == &ids[0]));
}
#[test]
fn marker_child() {
    if let Some(path) = std::env::var_os("TOPOLOGY_MARKER_CHILD") {
        let id = WorkspaceIdentity::discover(Some(Path::new(&path)), Path::new(&path)).unwrap();
        println!("MARKER_ID={}", id.record_id);
    }
}

#[test]
fn paused_creator_and_adopter_share_the_single_marker() {
    use std::sync::mpsc;
    use trellis::store::topology::MarkerStage;
    for initial in [b"".as_slice(), b"partial".as_slice()] {
        let (temp, _) = common::fixture();
        let work = root(temp.path());
        common::private(&work.join(".git"));
        let marker = work.join(".git/trellis/workspace-id");
        let (created_tx, created_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel();
        let (short_tx, short_rx) = mpsc::channel();
        let (retry_tx, retry_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let work_ref = &work;
            let creator = scope.spawn(move || {
                WorkspaceIdentity::discover_with_marker_hook(Some(work_ref), work_ref, |stage| {
                    if stage == MarkerStage::CreatedBeforeWrite {
                        created_tx.send(()).unwrap();
                        write_rx.recv().unwrap();
                    }
                    Ok(())
                })
                .unwrap()
            });
            created_rx.recv().unwrap();
            assert_eq!(fs::read(&marker).unwrap(), b"");
            fs::write(&marker, initial).unwrap();
            let adopter = scope.spawn(move || {
                WorkspaceIdentity::discover_with_marker_hook(Some(work_ref), work_ref, |stage| {
                    if stage == MarkerStage::ShortRead {
                        short_tx.send(()).unwrap();
                        retry_rx.recv().unwrap();
                    }
                    Ok(())
                })
                .unwrap()
            });
            short_rx.recv().unwrap();
            write_tx.send(()).unwrap();
            let winner = creator.join().unwrap();
            assert_eq!(fs::read(&marker).unwrap().len(), 36);
            retry_tx.send(()).unwrap();
            let loser = adopter.join().unwrap();
            assert_eq!(winner.record_id, loser.record_id);
            winner.verify().unwrap();
            loser.verify().unwrap();
        });
    }
}

#[test]
fn short_marker_disappearing_or_replaced_at_barrier_is_not_regenerated() {
    use trellis::store::topology::MarkerStage;
    for replacement in [None, Some("aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa")] {
        let (temp, _) = common::fixture();
        let work = root(temp.path());
        common::private(&work.join(".git"));
        let marker = work.join(".git/trellis/workspace-id");
        let original = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        fs::write(&marker, b"partial").unwrap();
        let mut saw_short = false;
        let error = WorkspaceIdentity::discover_with_marker_hook(Some(&work), &work, |stage| {
            if stage == MarkerStage::ShortRechecked {
                saw_short = true;
                fs::remove_file(&marker)?;
                if let Some(bytes) = replacement {
                    use std::os::unix::fs::PermissionsExt;
                    fs::write(&marker, bytes)?;
                    fs::set_permissions(&marker, fs::Permissions::from_mode(0o600))?;
                }
            }
            Ok(())
        })
        .unwrap_err();
        assert!(saw_short);
        assert!(!error.to_string().is_empty());
        match replacement {
            None => assert!(!marker.exists(), "a vanished short marker was regenerated"),
            Some(bytes) => assert_eq!(fs::read(&marker).unwrap(), bytes.as_bytes()),
        }
        assert!(original.verify().is_err());
    }
}

#[test]
fn every_marker_sync_stage_is_required_for_creator_and_adopter() {
    use trellis::store::topology::MarkerStage;
    for stage in [
        MarkerStage::MarkerSync,
        MarkerStage::PrivateDirSync,
        MarkerStage::GitDirSync,
    ] {
        let (temp, _) = common::fixture();
        let work = root(temp.path());
        common::private(&work.join(".git"));
        let marker = work.join(".git/trellis/workspace-id");
        let mut reached = vec![];
        let error = WorkspaceIdentity::discover_with_marker_hook(Some(&work), &work, |at| {
            reached.push(at);
            if at == stage {
                anyhow::bail!("injected {stage:?}")
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("workspace_id_not_durable"));
        assert_eq!(fs::read(&marker).unwrap().len(), 36);
        let mut adopted = vec![];
        let id = WorkspaceIdentity::discover_with_marker_hook(Some(&work), &work, |at| {
            adopted.push(at);
            Ok(())
        })
        .unwrap();
        assert_eq!(id.record_id, fs::read_to_string(&marker).unwrap());
        assert_eq!(
            adopted,
            [
                MarkerStage::MarkerSync,
                MarkerStage::PrivateDirSync,
                MarkerStage::GitDirSync
            ]
        );
        id.verify().unwrap();
    }
}

#[test]
fn persistent_short_and_malformed_markers_remain_unchanged() {
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let marker = work.join(".git/trellis/workspace-id");
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    for bytes in [
        b"partial".as_slice(),
        b"gaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa",
        b"aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaaextra",
    ] {
        fs::write(&marker, bytes).unwrap();
        assert!(WorkspaceIdentity::discover(Some(&work), &work).is_err());
        assert_eq!(fs::read(&marker).unwrap(), bytes);
    }
    fs::write(&marker, id.record_id.as_bytes()).unwrap();
    let original = id.record_id;
    assert_eq!(
        WorkspaceIdentity::discover(Some(&work), &work)
            .unwrap()
            .record_id,
        original
    );
    fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink(work.join(".git"), &marker).unwrap();
    assert!(WorkspaceIdentity::discover(Some(&work), &work).is_err());
}

#[test]
fn exclusive_recovery_reader_child() {
    let Some(path) = std::env::var_os("TOPOLOGY_RECOVERY_READER") else {
        return;
    };
    let guard = UseGuard::acquire_existing(Path::new(&path), false, true).unwrap();
    println!("READER_READY");
    std::io::stdout().flush().unwrap();
    let mut byte = [0];
    std::io::stdin().read_exact(&mut byte).unwrap();
    guard.verify().unwrap();
}

#[test]
fn external_write_destinations() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    assert!(roots.validate_external(&id, &[work.join("token")]).is_err());
    assert!(
        roots
            .validate_external(&id, &[temp.path().join("absent/../work/token")])
            .is_err()
    );
    assert!(
        roots
            .validate_external(&id, &[roots.data.join("token")])
            .is_err()
    );
    assert!(
        roots
            .validate_external(&id, &[temp.path().join("outside")])
            .is_ok()
    );
    assert!(
        roots
            .validate_external(
                &id,
                &[
                    temp.path().join("outside"),
                    temp.path().join("outside/child")
                ]
            )
            .is_err()
    );
    let another = temp.path().join("another");
    fs::create_dir(&another).unwrap();
    common::private(&another.join(".git"));
    assert!(
        roots
            .validate_external(&id, &[another.join("token")])
            .is_err()
    );
}

#[test]
fn marker_sync_faults_refuse_creator_and_adopter() {
    let (temp, _) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let fail = || anyhow::bail!("injected marker descriptor sync failure");
    let error =
        WorkspaceIdentity::discover_with_marker_sync_hook(Some(&work), &work, fail).unwrap_err();
    assert!(error.to_string().contains("workspace_id_not_durable"));
    let marker = work.join(".git/trellis/workspace-id");
    assert_eq!(fs::read(&marker).unwrap().len(), 36);
    let error =
        WorkspaceIdentity::discover_with_marker_sync_hook(Some(&work), &work, fail).unwrap_err();
    assert!(error.to_string().contains("workspace_id_not_durable"));
    WorkspaceIdentity::discover(Some(&work), &work)
        .unwrap()
        .verify()
        .unwrap();
}

#[test]
fn roots_overlapping_fixed_locations_are_refused_before_creation() {
    let (temp, roots) = common::fixture();
    fs::create_dir(&roots.cache).unwrap();
    fs::create_dir(&roots.data).unwrap();
    let alias = temp.path().join("cache-alias");
    std::os::unix::fs::symlink(&roots.cache, &alias).unwrap();
    let nested = roots.cache.join("nested");
    fs::create_dir(&nested).unwrap();
    let alias_nested = alias.join("nested");
    for candidate in [&roots.cache, &roots.data, temp.path(), &alias_nested] {
        let identity = WorkspaceIdentity::discover(Some(candidate), candidate).unwrap();
        assert!(
            roots
                .prepare_index(&identity)
                .unwrap_err()
                .to_string()
                .contains("overlaps")
        );
        assert!(
            roots
                .prepare_records(&identity)
                .unwrap_err()
                .to_string()
                .contains("overlaps")
        );
        assert!(!roots.cache.join("indexes").exists());
        assert!(!roots.data.join("workspaces").exists());
    }
    let id = WorkspaceIdentity::discover(Some(&nested), &nested).unwrap();
    assert!(roots.prepare_index(&id).is_err());
    assert!(!roots.cache.join("indexes").exists());
}

#[test]
fn shared_use_cannot_remove_last_and_live_holder_blocks_exclusive() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let shared = roots.index_use(&identity).unwrap();
    let path = roots.index_use_lock(&identity);
    assert!(
        UseGuard::acquire(&path, false, false)
            .unwrap()
            .remove_last()
            .unwrap_err()
            .to_string()
            .contains("exclusive")
    );
    assert!(
        UseGuard::acquire(&path, true, true)
            .unwrap_err()
            .to_string()
            .contains("storage_busy")
    );
    assert!(path.exists());
    shared.verify().unwrap();
    drop(shared);
    // Parallel process fixtures may briefly inherit a test lock during spawn.
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let exclusive = loop {
        match UseGuard::acquire(&path, true, true) {
            Ok(guard) => break guard,
            Err(error)
                if error.to_string().starts_with("storage_busy")
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now()
            }
            Err(error) => panic!("exclusive lock remained busy after releasing holders: {error}"),
        }
    };
    exclusive.remove_last().unwrap();
}

#[test]
fn stale_use_inode_waiters_reopen_before_success() {
    use std::io::{BufRead, BufReader};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    roots.prepare_index(&identity).unwrap();
    let path = roots.index_use_lock(&identity);
    let original = UseGuard::acquire(&path, true, true).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("stale_use_child")
        .arg("--nocapture")
        .env("TOPOLOGY_STALE_USE", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line.contains("OLD_OPEN") {
            break;
        }
    }
    original.remove_last().unwrap();
    let replacement = UseGuard::acquire(&path, true, true).unwrap();
    child.stdin.take().unwrap().write_all(&[1]).unwrap();
    // The child first locks the unlinked inode, then must wait for the new one.
    assert!(child.try_wait().unwrap().is_none());
    drop(replacement);
    assert!(child.wait().unwrap().success());
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert!(rest.contains("NEW_INODE_VERIFIED"), "{rest}");
}

#[test]
fn stale_use_child() {
    if let Some(path) = std::env::var_os("TOPOLOGY_STALE_USE") {
        let path = Path::new(&path);
        let guard = UseGuard::acquire_with_hook(path, false, false, || {
            println!("OLD_OPEN");
            std::io::stdout().flush()?;
            let mut byte = [0];
            std::io::stdin().read_exact(&mut byte)?;
            Ok(())
        })
        .unwrap();
        guard.verify().unwrap();
        println!("NEW_INODE_VERIFIED");
    }
}
#[test]
fn exclusive_deletion_waits_for_shared_process() {
    use std::io::{BufRead, BufReader};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    roots.prepare_index(&id).unwrap();
    let path = roots.index_use_lock(&id);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("shared_holder_child")
        .arg("--nocapture")
        .env("TOPOLOGY_SHARED_HOLDER", &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line.contains("SHARED_HELD") {
            break;
        }
    }
    assert!(
        UseGuard::acquire(&path, true, true)
            .unwrap_err()
            .to_string()
            .contains("storage_busy")
    );
    assert!(path.exists());
    child.stdin.take().unwrap().write_all(&[1]).unwrap();
    assert!(child.wait().unwrap().success());
    UseGuard::acquire(&path, true, true)
        .unwrap()
        .remove_last()
        .unwrap();
    assert!(!path.exists());
}
#[test]
fn shared_holder_child() {
    if let Some(path) = std::env::var_os("TOPOLOGY_SHARED_HOLDER") {
        let guard = UseGuard::acquire(Path::new(&path), false, false).unwrap();
        println!("SHARED_HELD");
        std::io::stdout().flush().unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        guard.verify().unwrap();
    }
}
use std::os::unix::fs::PermissionsExt;

#[test]
fn durable_first_save_empty_record_and_payloads() {
    use trellis::{
        model::{Annotation, SavedView},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &id);
    assert!(records.views().unwrap().is_empty());
    assert!(records.annotations().unwrap().is_empty());
    assert!(!records.delete_view("missing").unwrap());
    assert!(!roots.data.exists(), "read-only access created data root");
    let view: SavedView = serde_json::from_str(r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{"symbol":{"x":12.5,"y":9.25}}}"#).unwrap();
    records.put_view(&view).unwrap();
    let annotation = Annotation {
        id: "note1".into(),
        node_id: "symbol".into(),
        body: "Original text".into(),
    };
    records.put_annotation(&annotation).unwrap();
    assert_eq!(records.view("view1").unwrap(), Some(view.clone()));
    assert_eq!(records.annotations().unwrap(), vec![annotation.clone()]);
    assert!(records.delete_view("view1").unwrap());
    assert!(records.delete_annotation("note1").unwrap());
    assert!(records.views().unwrap().is_empty());
    assert!(roots.record_db(&id).exists());
    let db = rusqlite::Connection::open(roots.record_db(&id)).unwrap();
    assert_eq!(
        db.query_row("SELECT record_id FROM record_metadata", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        id.record_id
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM known_roots", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        tables,
        ["annotations", "known_roots", "record_metadata", "views"]
    );
    assert!(
        !roots
            .record_db(&id)
            .with_file_name("workspace.db-wal")
            .exists()
    );
}

#[test]
fn durable_git_move_and_copy_share_payload_without_rewriting() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let original = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let note = Annotation {
        id: "note".into(),
        node_id: "gone".into(),
        body: "Persist verbatim".into(),
    };
    DurableRecords::new(&roots, &original)
        .put_annotation(&note)
        .unwrap();
    let copied = temp.path().join("copy");
    fs::create_dir(&copied).unwrap();
    common::private(&copied.join(".git"));
    fs::create_dir(copied.join(".git/trellis")).unwrap();
    fs::set_permissions(
        copied.join(".git/trellis"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::copy(
        work.join(".git/trellis/workspace-id"),
        copied.join(".git/trellis/workspace-id"),
    )
    .unwrap();
    let copy = WorkspaceIdentity::discover(Some(&copied), &copied).unwrap();
    assert_eq!(copy.record_id, original.record_id);
    assert_eq!(
        DurableRecords::new(&roots, &copy).annotations().unwrap(),
        vec![note.clone()]
    );
    let moved = temp.path().join("moved");
    fs::rename(&work, &moved).unwrap();
    let moved_id = WorkspaceIdentity::discover(Some(&moved), &moved).unwrap();
    DurableRecords::new(&roots, &moved_id)
        .put_annotation(&note)
        .unwrap();
    assert_eq!(
        DurableRecords::new(&roots, &copy).annotations().unwrap(),
        vec![note.clone()]
    );
    assert!(
        DurableRecords::new(&roots, &original)
            .annotations()
            .is_err()
    );
    let db = rusqlite::Connection::open(roots.record_db(&copy)).unwrap();
    let rows: Vec<(String, String, String)> = db
        .prepare("SELECT path,device,inode FROM known_roots ORDER BY path")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut expected = vec![
        (
            original.root.to_str().unwrap().to_owned(),
            original.device.to_string(),
            original.inode.to_string(),
        ),
        (
            moved_id.root.to_str().unwrap().to_owned(),
            moved_id.device.to_string(),
            moved_id.inode.to_string(),
        ),
    ];
    expected.sort();
    assert_eq!(rows, expected);
    let payload: String = db
        .query_row("SELECT payload FROM annotations WHERE id='note'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(payload, serde_json::to_string(&note).unwrap());
}

#[test]
fn durable_incomplete_and_incompatible_refused() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    roots.prepare_records(&id).unwrap();
    common::private(&roots.record_dir(&id));
    let note = Annotation {
        id: "note".into(),
        node_id: "node".into(),
        body: "Body".into(),
    };
    assert!(
        DurableRecords::new(&roots, &id)
            .put_annotation(&note)
            .is_err()
    );
    assert!(!roots.record_db(&id).exists());
    let _lock = roots.record_use(&id, true).unwrap();
    let db = rusqlite::Connection::open(roots.record_db(&id)).unwrap();
    db.pragma_update(None, "user_version", 99).unwrap();
    drop(db);
    drop(_lock);
    assert!(DurableRecords::new(&roots, &id).annotations().is_err());
}

#[test]
fn durable_absent_parent_is_verified() {
    use trellis::store::topology::DurableRecords;
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    fs::create_dir(&roots.data).unwrap();
    fs::set_permissions(&roots.data, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(DurableRecords::new(&roots, &id).views().is_err());
    fs::set_permissions(&roots.data, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(roots.data.join("workspaces")).unwrap();
    fs::set_permissions(
        roots.data.join("workspaces"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(
        DurableRecords::new(&roots, &id)
            .delete_view("absent")
            .is_err()
    );
    fs::remove_dir(roots.data.join("workspaces")).unwrap();
    std::os::unix::fs::symlink(temp.path(), roots.data.join("workspaces")).unwrap();
    assert!(DurableRecords::new(&roots, &id).annotations().is_err());
}

#[test]
fn durable_existing_lock_unlink_never_recreates() {
    use trellis::store::topology::DurableRecords;
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let note = trellis::model::Annotation {
        id: "n".into(),
        node_id: "node".into(),
        body: "body".into(),
    };
    DurableRecords::new(&roots, &id)
        .put_annotation(&note)
        .unwrap();
    let lock = roots.record_use_lock(&id);
    assert!(
        UseGuard::acquire_existing_with_hook(&lock, false, false, || {
            fs::remove_file(&lock)?;
            Ok(())
        })
        .is_err()
    );
    assert!(!lock.exists(), "existing-only reopen created a lock");
    assert!(DurableRecords::new(&roots, &id).annotations().is_err());
    assert!(!lock.exists(), "durable read created a lock");
    assert!(
        DurableRecords::new(&roots, &id)
            .delete_annotation("n")
            .is_err()
    );
    assert!(!lock.exists(), "durable delete created a lock");
}

#[test]
fn durable_existing_lock_replacement_checks_new_inode_and_holder() {
    use std::io::{BufRead, BufReader};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    roots.prepare_records(&id).unwrap();
    let lock = roots.record_use_lock(&id);
    drop(roots.record_use(&id, true).unwrap());
    let mut child = None;
    let mut reader = None;
    let result = UseGuard::acquire_existing_with_hook(&lock, false, true, || {
        fs::remove_file(&lock)?;
        drop(roots.record_use(&id, true)?);
        let mut process = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("shared_holder_child")
            .arg("--nocapture")
            .env("TOPOLOGY_SHARED_HOLDER", &lock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut output = BufReader::new(process.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(output.read_line(&mut line)?, 0);
            if line.contains("SHARED_HELD") {
                break;
            }
        }
        reader = Some(output);
        child = Some(process);
        Ok(())
    });
    // The replacement is held shared. An exclusive acquisition must not use the old inode.
    assert!(result.is_ok());
    let process = child.as_mut().unwrap();
    assert!(
        UseGuard::acquire_existing(&lock, true, true)
            .unwrap_err()
            .to_string()
            .contains("storage_busy")
    );
    process.stdin.take().unwrap().write_all(&[1]).unwrap();
    assert!(process.wait().unwrap().success());
}

#[test]
fn durable_annotation_first_commit_fault_and_sync_fault() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let note = Annotation {
        id: "first".into(),
        node_id: "node".into(),
        body: "original".into(),
    };
    let records = DurableRecords::new(&roots, &id);
    assert!(
        records
            .put_annotation_with_first_save_hook(&note, |phase| {
                if phase == "before_commit" {
                    anyhow::bail!("injected commit fault")
                }
                Ok(())
            })
            .unwrap_err()
            .to_string()
            .contains("injected commit fault")
    );
    assert!(
        records
            .annotations()
            .unwrap_err()
            .to_string()
            .contains("incomplete_record")
    );
    assert!(
        records.put_annotation(&note).is_err(),
        "must not heal incomplete record"
    );
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &id);
    assert!(
        records
            .put_annotation_with_first_save_hook(&note, |phase| {
                if phase == "before_sync" {
                    anyhow::bail!("injected sync fault")
                }
                Ok(())
            })
            .unwrap_err()
            .to_string()
            .contains("injected sync fault")
    );
    // A committed record is not silently erased or described as incomplete after sync refusal.
    assert_eq!(records.annotations().unwrap(), vec![note]);
}

#[test]
fn durable_creator_child() {
    if let Some(base) = std::env::var_os("TOPOLOGY_DURABLE_CHILD") {
        use trellis::{model::Annotation, store::topology::DurableRecords};
        let base = Path::new(&base);
        let roots = trellis::store::topology::TopologyRoots::isolated_for_tests(
            base.join("cache"),
            base.join("data"),
        );
        let work = base.join("work");
        let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        let note = Annotation {
            id: "loser".into(),
            node_id: "node".into(),
            body: "second".into(),
        };
        assert!(
            DurableRecords::new(&roots, &id)
                .put_annotation(&note)
                .unwrap_err()
                .to_string()
                .contains("storage_busy")
        );
        println!("CREATOR_BUSY");
        std::io::stdout().flush().unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        DurableRecords::new(&roots, &id)
            .put_annotation(&note)
            .unwrap();
        println!("CREATOR_RETRIED");
    }
}

#[test]
fn durable_first_creator_process_contention_and_retry() {
    use std::io::{BufRead, BufReader};
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let lock = roots.record_use(&id, true).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("durable_creator_child")
        .arg("--nocapture")
        .env("TOPOLOGY_DURABLE_CHILD", temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if line.contains("CREATOR_BUSY") {
            break;
        }
    }
    drop(lock);
    let first = Annotation {
        id: "winner".into(),
        node_id: "node".into(),
        body: "first".into(),
    };
    DurableRecords::new(&roots, &id)
        .put_annotation(&first)
        .unwrap();
    child.stdin.take().unwrap().write_all(&[1]).unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        DurableRecords::new(&roots, &id)
            .annotations()
            .unwrap()
            .len(),
        2
    );
    let db = rusqlite::Connection::open(roots.record_db(&id)).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM record_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn durable_non_git_move_retains_old_record_and_root_row() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let old = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let note = Annotation {
        id: "n".into(),
        node_id: "node".into(),
        body: "unchanged".into(),
    };
    DurableRecords::new(&roots, &old)
        .put_annotation(&note)
        .unwrap();
    let old_db = roots.record_db(&old);
    fs::rename(&work, temp.path().join("moved")).unwrap();
    let moved_path = temp.path().join("moved");
    let moved = WorkspaceIdentity::discover(Some(&moved_path), &moved_path).unwrap();
    assert_ne!(old.record_id, moved.record_id);
    assert!(
        DurableRecords::new(&roots, &moved)
            .annotations()
            .unwrap()
            .is_empty()
    );
    assert!(old_db.exists());
    let db = rusqlite::Connection::open(&old_db).unwrap();
    let saved: (String, String, String) = db
        .query_row("SELECT path,device,inode FROM known_roots", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(
        saved,
        (
            old.root.to_str().unwrap().into(),
            old.device.to_string(),
            old.inode.to_string()
        )
    );
    let payload: String = db
        .query_row("SELECT payload FROM annotations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(serde_json::from_str::<Annotation>(&payload).unwrap(), note);
}

#[test]
fn durable_marker_change_refuses_real_operation() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let note = Annotation {
        id: "n".into(),
        node_id: "node".into(),
        body: "body".into(),
    };
    DurableRecords::new(&roots, &id)
        .put_annotation(&note)
        .unwrap();
    fs::write(
        work.join(".git/trellis/workspace-id"),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    assert!(DurableRecords::new(&roots, &id).annotations().is_err());
    assert!(
        DurableRecords::new(&roots, &id)
            .put_annotation(&note)
            .is_err()
    );
    assert!(
        DurableRecords::new(&roots, &id)
            .delete_annotation("n")
            .is_err()
    );
}

#[test]
fn index_open_and_generation() {
    let base = tempfile::tempdir().unwrap();
    let work = root(base.path());
    let state = base.path().join("state");
    let store = common::open_store(&state, &work).unwrap();
    let first = store.index_baseline().unwrap();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    assert_eq!(first.index_revision, 0);
    assert_eq!(first.index_generation.get_version_num(), 4);
    drop(store);
    let reopened = common::open_store(&state, &work).unwrap();
    assert_eq!(reopened.index_baseline().unwrap(), first);
}
#[test]
fn index_delete_journal_no_wal() {
    let base = tempfile::tempdir().unwrap();
    let work = root(base.path());
    let state = base.path().join("state");
    let store = common::open_store(&state, &work).unwrap();
    let index = fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db =
        rusqlite::Connection::open_with_flags(&index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let journal: String = db
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(journal, "delete");
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        21
    );
    drop(db);
    let bytes = fs::read(&index).unwrap();
    assert_eq!((bytes[18], bytes[19]), (1, 1));
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!index.with_file_name(format!("index.db{suffix}")).exists());
    }
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
}
#[cfg(feature = "test-support")]
#[test]
fn writer_records_open_age_and_reopen_preserves_it() {
    let base = tempfile::tempdir().unwrap();
    let work = root(base.path());
    let state = base.path().join("state");
    let store = common::open_store(&state, &work).unwrap();
    let index = fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let read_age = || -> i64 {
        rusqlite::Connection::open(&index)
            .unwrap()
            .query_row("SELECT last_opened_at FROM index_metadata", [], |r| {
                r.get(0)
            })
            .unwrap()
    };
    assert!(read_age() > 0);
    let baseline = store.index_baseline().unwrap();
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let writer = store.publication_for_tests().unwrap();
    let age = read_age();
    assert!(age > 0);
    drop(writer);
    drop(store);
    let reopened = common::open_store(&state, &work).unwrap();
    assert_eq!(read_age(), age);
    assert_eq!(reopened.index_baseline().unwrap(), baseline);
    let writer = reopened.publication_for_tests().unwrap();
    assert!(read_age() >= age);
    assert_eq!(reopened.index_baseline().unwrap(), baseline);
    drop(writer);
}

#[cfg(feature = "test-support")]
#[test]
fn guard_drop_unlocks_even_when_fork_child_keeps_descriptor() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(roots.index_use(&id).unwrap());
    let guard = roots
        .writer(std::sync::Arc::new(id.verified_clone_for_tests().unwrap()))
        .unwrap();
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    // Close-on-exec: a process spawned by a concurrent test must not inherit
    // the write end and hold off the child's EOF (#131). fork still shares it.
    for fd in pipe {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        unsafe { libc::close(pipe[1]) };
        let mut byte = 0u8;
        unsafe { libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1) };
        unsafe { libc::_exit(0) };
    }
    unsafe { libc::close(pipe[0]) };
    drop(guard);
    let exclusive = UseGuard::acquire_existing(&roots.index_use_lock(&id), true, true).unwrap();
    drop(exclusive);
    let next = roots
        .writer(std::sync::Arc::new(id.verified_clone_for_tests().unwrap()))
        .unwrap();
    drop(next);
    unsafe { libc::close(pipe[1]) };
    let mut status = 0;
    assert_eq!(unsafe { libc::waitpid(child, &mut status, 0) }, child);
    assert_eq!(status, 0);
}

type GcManifestEntry = (std::path::PathBuf, Option<Vec<u8>>, u64, i64, i64);
fn gc_manifest(base: &Path) -> Vec<GcManifestEntry> {
    use std::os::unix::fs::MetadataExt;
    fn walk(base: &Path, path: &Path, result: &mut Vec<GcManifestEntry>) {
        if !path.exists() {
            return;
        }
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            let stat = fs::symlink_metadata(&entry).unwrap();
            result.push((
                entry.strip_prefix(base).unwrap().to_owned(),
                stat.is_file().then(|| fs::read(&entry).unwrap()),
                stat.len(),
                stat.mtime(),
                stat.mtime_nsec(),
            ));
            if stat.is_dir() {
                walk(base, &entry, result);
            }
        }
    }
    let mut result = Vec::new();
    walk(base, base, &mut result);
    result
}

#[test]
fn gc_age_and_record_inventory_are_read_only() {
    use std::os::unix::fs::MetadataExt;
    use trellis::model::SavedView;
    use trellis::store::topology::{DurableRecords, classify_index_age, valid_record_id};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let now = 1_800_000_000;
    assert_eq!(
        classify_index_age(now, Some(now - 2_591_999)),
        ("unknown", "recent_open")
    );
    assert_eq!(
        classify_index_age(now, Some(now - 2_592_000)),
        ("eligible", "age_30_days")
    );
    assert_eq!(
        classify_index_age(now, Some(now - 2_592_001)),
        ("eligible", "age_30_days")
    );
    for age in [None, Some(0), Some(-1), Some(now + 1), Some(i64::MAX)] {
        assert_eq!(classify_index_age(now, age), ("unknown", "age_unknown"));
    }
    for invalid in [
        "",
        "../outside",
        "path-0",
        "00000000-0000-0000-0000-000000000000",
        "PATH-0000000000000000000000000000000000000000000000000000000000000000",
    ] {
        assert!(!valid_record_id(invalid));
        assert!(roots.record_by_id(invalid).is_err());
    }
    let empty = roots.gc_report_at(now).unwrap();
    assert!(empty.derived.is_empty() && empty.records.is_empty());
    assert!(!roots.cache.exists() && !roots.data.exists());
    let store = common::open_store(temp.path(), &work).unwrap();
    drop(store);
    let db_path = roots.index_db(&identity);
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 2_592_000],
        )
        .unwrap();
    }
    for (age, expected) in [
        (now - 2_591_999, ("unknown", "recent_open")),
        (now - 2_592_000, ("eligible", "age_30_days")),
        (now - 2_592_001, ("eligible", "age_30_days")),
        (0, ("unknown", "age_unknown")),
        (now + 1, ("unknown", "age_unknown")),
    ] {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute("UPDATE index_metadata SET last_opened_at=?1", [age])
            .unwrap();
        drop(db);
        let report = roots.gc_report_at(now).unwrap();
        assert_eq!(
            (report.derived[0].status, report.derived[0].reason),
            expected
        );
    }
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        db.execute("UPDATE index_metadata SET last_opened_at='invalid'", [])
            .unwrap();
    }
    assert_eq!(
        roots.gc_report_at(now).unwrap().derived[0].reason,
        "age_unknown"
    );
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 2_592_000],
        )
        .unwrap();
    }
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    let records = DurableRecords::new(&roots, &identity);
    records.put_view(&view).unwrap();
    records.delete_view("view1").unwrap();
    let record_db = roots.record_db(&identity);
    let before = [db_path.as_path(), record_db.as_path()].map(|path| {
        let stat = fs::metadata(path).unwrap();
        (
            fs::read(path).unwrap(),
            stat.len(),
            stat.mtime(),
            stat.mtime_nsec(),
        )
    });
    let manifest_before = gc_manifest(temp.path());
    let report = roots.gc_report_at(now).unwrap();
    assert_eq!(manifest_before, gc_manifest(temp.path()));
    assert_eq!(
        (report.derived[0].status, report.derived[0].reason),
        ("eligible", "age_30_days")
    );
    assert_eq!(report.records.len(), 1);
    assert_eq!(
        (report.records[0].views, report.records[0].annotations),
        (Some(0), Some(0))
    );
    assert_eq!(report.records[0].missing_known_paths, Some(vec![]));
    assert_eq!(
        roots.record_by_id(&identity.record_id).unwrap().unwrap().id,
        identity.record_id
    );
    let after = [db_path.as_path(), record_db.as_path()].map(|path| {
        let stat = fs::metadata(path).unwrap();
        (
            fs::read(path).unwrap(),
            stat.len(),
            stat.mtime(),
            stat.mtime_nsec(),
        )
    });
    assert_eq!(before, after);
    let held = UseGuard::acquire_existing(&roots.index_use_lock(&identity), false, false).unwrap();
    assert_eq!(roots.gc_report_at(now).unwrap().derived[0].status, "busy");
    drop(held);
    fs::rename(&work, temp.path().join("moved")).unwrap();
    let missing = roots.gc_report_at(now).unwrap();
    assert_eq!(missing.derived[0].reason, "root_missing");
    assert_eq!(
        missing.records[0].missing_known_paths,
        Some(vec![identity.root.to_string_lossy().to_string()])
    );
    fs::create_dir(&work).unwrap();
    assert_eq!(
        roots.gc_report_at(now).unwrap().derived[0].reason,
        "root_replaced"
    );
    let backup = db_path.with_file_name("index.backup");
    fs::rename(&db_path, &backup).unwrap();
    assert_eq!(
        roots.gc_report_at(now).unwrap().derived[0].reason,
        "metadata_unreadable"
    );
    fs::rename(&backup, &db_path).unwrap();
    let lock = roots.index_use_lock(&identity);
    let backup = lock.with_extension("backup");
    fs::rename(&lock, &backup).unwrap();
    std::os::unix::fs::symlink(&backup, &lock).unwrap();
    assert_eq!(
        roots.gc_report_at(now).unwrap().derived[0].reason,
        "unsafe_use_lock"
    );
    fs::remove_file(&lock).unwrap();
    fs::rename(&backup, &lock).unwrap();
}

#[test]
fn gc_rejects_dangling_recovery_sidecars_without_writes() {
    use std::os::unix::fs::symlink;
    use trellis::model::Annotation;
    use trellis::store::topology::DurableRecords;
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(common::open_store(temp.path(), &work).unwrap());
    DurableRecords::new(&roots, &identity)
        .put_annotation(&Annotation {
            id: "note".into(),
            node_id: "node".into(),
            body: "saved".into(),
        })
        .unwrap();
    for (db, is_record) in [
        (roots.index_db(&identity), false),
        (roots.record_db(&identity), true),
    ] {
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = db.with_file_name(format!(
                "{}{suffix}",
                db.file_name().unwrap().to_string_lossy()
            ));
            symlink(temp.path().join("absent"), &sidecar).unwrap();
            assert!(fs::symlink_metadata(&sidecar).is_ok());
            assert!(!sidecar.exists(), "fixture must be a dangling link");
            let before = gc_manifest(temp.path());
            if is_record {
                assert!(roots.record_by_id(&identity.record_id).is_err(), "{suffix}");
                let report = roots.gc_report_at(1_800_000_000).unwrap();
                assert_eq!(report.records[0].id, identity.record_id);
                assert_eq!(
                    (report.records[0].status, report.records[0].reason),
                    ("unknown", "recovery_sidecar"),
                    "{suffix}"
                );
                assert_eq!(report.records[0].views, None);
            } else {
                let report = roots.gc_report_at(1_800_000_000).unwrap();
                assert_eq!(
                    (report.derived[0].status, report.derived[0].reason),
                    ("unknown", "metadata_unreadable"),
                    "{suffix}"
                );
            }
            assert_eq!(
                gc_manifest(temp.path()),
                before,
                "report wrote with {suffix}"
            );
            fs::remove_file(sidecar).unwrap();
        }
    }
}

#[test]
fn gc_refuses_countable_records_with_missing_durable_columns() {
    use trellis::model::Annotation;
    use trellis::store::topology::DurableRecords;
    let (temp, roots) = common::fixture();
    for (n, table, column) in [
        (0, "known_roots", "device"),
        (1, "known_roots", "inode"),
        (2, "views", "payload"),
        (3, "annotations", "node_id"),
        (4, "annotations", "payload"),
    ] {
        let work = temp.path().join(format!("record-{n}"));
        fs::create_dir(&work).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        DurableRecords::new(&roots, &identity)
            .put_annotation(&Annotation {
                id: "note".into(),
                node_id: "node".into(),
                body: "saved".into(),
            })
            .unwrap();
        let db = rusqlite::Connection::open(roots.record_db(&identity)).unwrap();
        db.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column}"))
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM annotations", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(db);
        assert!(
            roots.record_by_id(&identity.record_id).is_err(),
            "{table}.{column}"
        );
        let report = roots.gc_report_at(1_800_000_000).unwrap();
        let entry = report
            .records
            .iter()
            .find(|r| r.id == identity.record_id)
            .unwrap();
        assert_eq!(
            (entry.status, entry.reason),
            ("unknown", "incompatible_record"),
            "{table}.{column}"
        );
        assert_eq!(entry.views, None);
    }
}

#[test]
fn gc_report_sorts_multiple_derived_records_and_missing_paths() {
    use trellis::model::Annotation;
    use trellis::store::topology::DurableRecords;
    let (temp, roots) = common::fixture();
    let mut identities = (0..3)
        .map(|n| {
            let work = temp.path().join(format!("sort-{n}"));
            fs::create_dir(&work).unwrap();
            WorkspaceIdentity::discover(Some(&work), &work).unwrap()
        })
        .collect::<Vec<_>>();
    identities.sort_by(|a, b| b.root_key.cmp(&a.root_key));
    let insertion_keys: Vec<_> = identities.iter().map(|id| id.root_key.clone()).collect();
    let insertion_ids: Vec<_> = identities.iter().map(|id| id.record_id.clone()).collect();
    for identity in &identities {
        drop(common::open_store(temp.path(), &identity.root).unwrap());
        DurableRecords::new(&roots, identity)
            .put_annotation(&Annotation {
                id: "note".into(),
                node_id: "node".into(),
                body: "saved".into(),
            })
            .unwrap();
        let db = rusqlite::Connection::open(roots.record_db(identity)).unwrap();
        for name in ["z", "m", "a"] {
            db.execute(
                "INSERT INTO known_roots(path,device,inode) VALUES(?1,'1','1')",
                [temp
                    .path()
                    .join(format!("missing-{}-{name}", identity.record_id))
                    .to_str()
                    .unwrap()],
            )
            .unwrap();
        }
    }
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    let mut expected_keys = insertion_keys.clone();
    expected_keys.sort();
    let mut expected_ids = insertion_ids.clone();
    expected_ids.sort();
    assert_eq!(
        report
            .derived
            .iter()
            .map(|entry| entry.root_key.clone())
            .collect::<Vec<_>>(),
        expected_keys
    );
    assert_eq!(
        report
            .records
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>(),
        expected_ids
    );
    for record in &report.records {
        let paths = record.missing_known_paths.as_ref().unwrap();
        let mut expected = paths.clone();
        expected.sort();
        assert_eq!(paths, &expected);
        assert_eq!(paths.len(), 3);
        assert!(paths[0].ends_with("-a"));
    }
}

#[test]
fn forget_requires_safe_exclusive_record_and_recreates_on_later_save() {
    use trellis::{
        model::SavedView,
        store::topology::{DurableRecords, valid_record_id},
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let id = identity.record_id.clone();
    let copy = temp.path().join("copy");
    fs::create_dir(&copy).unwrap();
    common::private(&copy.join(".git"));
    common::private(&copy.join(".git/trellis"));
    fs::copy(
        work.join(".git/trellis/workspace-id"),
        copy.join(".git/trellis/workspace-id"),
    )
    .unwrap();
    let copied = WorkspaceIdentity::discover(Some(&copy), &copy).unwrap();
    assert_eq!(copied.record_id, id);
    assert!(valid_record_id(&id));
    for bad in [
        "",
        "../bad",
        "00000000-0000-0000-0000-000000000000",
        "PATH-bad",
        "path-ABC",
        "ABCDEFAB-CDEF-4ABC-ABCD-ABCDEFABCDEF",
    ] {
        assert!(!valid_record_id(bad));
        assert!(
            roots
                .forget_with_confirmation(bad, |_, _| Ok(true))
                .is_err()
        );
    }
    let records = DurableRecords::new(&roots, &identity);
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    records.put_view(&view).unwrap();
    let directory = roots.record_dir(&identity);
    let lock = roots.record_use_lock(&identity);
    let shared = UseGuard::acquire_existing(&lock, false, true).unwrap();
    assert!(
        roots
            .forget_with_confirmation(&id, |_, _| Ok(true))
            .is_err()
    );
    drop(shared);
    assert!(
        !roots
            .forget_with_confirmation(&id, |report, paths| {
                assert_eq!(report.views, 1);
                assert_eq!(report.annotations, 0);
                assert_eq!(paths, &[identity.root.to_str().unwrap().to_string()]);
                Ok(false)
            })
            .unwrap()
    );
    assert!(directory.exists());
    fs::write(directory.join("unknown"), "leave untouched").unwrap();
    assert!(
        roots
            .forget_with_confirmation(&id, |_, _| Ok(true))
            .is_err()
    );
    assert!(directory.join("unknown").exists());
    fs::remove_file(directory.join("unknown")).unwrap();
    assert!(
        roots
            .forget_with_confirmation(&id, |_, _| Ok(true))
            .unwrap()
    );
    assert!(!directory.exists());
    assert!(!lock.exists());
    assert!(work.join(".git/trellis/workspace-id").exists());
    assert!(copy.join(".git/trellis/workspace-id").exists());
    assert!(
        DurableRecords::new(&roots, &copied)
            .views()
            .unwrap()
            .is_empty()
    );
    records.put_view(&view).unwrap();
    assert_eq!(records.views().unwrap(), vec![view]);
}

#[test]
fn forget_empty_record_and_recovery_sidecars_refuse() {
    use trellis::{model::SavedView, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    records.put_view(&view).unwrap();
    records.delete_view("view1").unwrap();
    let db = roots.record_db(&identity);
    fs::write(db.with_file_name("workspace.db-wal"), "recovery").unwrap();
    assert!(
        roots
            .forget_with_confirmation(&identity.record_id, |_, _| Ok(true))
            .is_err()
    );
    assert!(db.exists());
    fs::remove_file(db.with_file_name("workspace.db-wal")).unwrap();
    assert!(
        roots
            .forget_with_confirmation(&identity.record_id, |report, _| {
                assert_eq!(report.views, 0);
                Ok(true)
            })
            .unwrap()
    );
}

#[test]
fn forget_refuses_countable_records_with_unknown_sqlite_schema() {
    use trellis::{model::SavedView, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    for (n, change) in [
        "CREATE TABLE unexpected (id INTEGER)",
        "CREATE VIEW unexpected AS SELECT id FROM views",
        "CREATE TRIGGER unexpected AFTER INSERT ON views BEGIN SELECT 1; END",
        "CREATE INDEX unexpected ON views(payload)",
        "ALTER TABLE views ADD COLUMN unexpected TEXT",
        "UPDATE sqlite_master SET sql=replace(sql,'CHECK(schema_version=1)','CHECK(schema_version>0)') WHERE name='record_metadata'",
    ].iter().enumerate() {
        let work = temp.path().join(format!("unsafe-schema-{n}"));
        fs::create_dir(&work).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        DurableRecords::new(&roots, &identity).put_view(&view).unwrap();
        let path = roots.record_db(&identity);
        let lock = roots.record_use_lock(&identity);
        let db = rusqlite::Connection::open(&path).unwrap();
        if n == 5 {
            db.pragma_update(None, "writable_schema", "ON").unwrap();
        }
        db.execute_batch(change).unwrap();
        if n == 5 {
            db.pragma_update(None, "writable_schema", "OFF").unwrap();
            db.pragma_update(None, "schema_version", 100).unwrap();
        }
        assert_eq!(db.query_row("SELECT count(*) FROM views", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        drop(db);
        let before_db = fs::read(&path).unwrap();
        let before_lock = fs::read(&lock).unwrap();
        let unrelated = temp.path().join(format!("unrelated-{n}"));
        fs::write(&unrelated, "keep").unwrap();
        let before_unrelated = fs::read(&unrelated).unwrap();
        let mut asked = false;
        let result = roots.forget_with_confirmation(&identity.record_id, |_, _| {
            asked = true;
            Ok(true)
        });
        assert!(result.is_err(), "{change}");
        assert!(!asked, "must reject before confirmation: {change}");
        assert_eq!(fs::read(&path).unwrap(), before_db, "{change}");
        assert_eq!(fs::read(&lock).unwrap(), before_lock, "{change}");
        assert_eq!(fs::read(&unrelated).unwrap(), before_unrelated, "{change}");
    }
}

#[test]
fn forget_rechecks_sqlite_schema_after_confirmation() {
    use trellis::{model::SavedView, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    DurableRecords::new(&roots, &identity)
        .put_view(&view)
        .unwrap();
    let path = roots.record_db(&identity);
    let lock = roots.record_use_lock(&identity);
    assert!(
        roots
            .forget_with_confirmation(&identity.record_id, |_, _| {
                let db = rusqlite::Connection::open(&path)?;
                db.execute_batch("CREATE TABLE late_entry(id INTEGER)")?;
                Ok(true)
            })
            .is_err()
    );
    assert!(path.exists() && lock.exists());
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM views", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn gc_classifies_current_schema21_but_refuses_spoofed_shapes() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(common::open_store(temp.path(), &work).unwrap());
    let index = roots.index_db(&identity);
    let now = 1_800_000_000_i64;
    let db = rusqlite::Connection::open(&index).unwrap();
    db.execute("UPDATE index_metadata SET last_opened_at=?1", [now])
        .unwrap();
    let inspect = || {
        let before = gc_manifest(temp.path());
        let entry = roots.gc_report_at(now).unwrap().derived.remove(0);
        // The full inventory records every pathname plus each file's bytes:
        // equality rules out both mutation and a covert derived-index deletion.
        assert_eq!(
            before,
            gc_manifest(temp.path()),
            "GC report must not mutate or delete any index bytes"
        );
        (entry.status, entry.reason)
    };
    assert_eq!(inspect(), ("unknown", "recent_open"));
    // The current CHECK forbids spoofing an old extractor on the current layout.
    assert!(
        db.execute(
            "UPDATE index_metadata SET extractor_version='native-v1'",
            []
        )
        .is_err()
    );
    assert_eq!(inspect(), ("unknown", "recent_open"));
    db.execute_batch("CREATE TABLE unsupported(id INTEGER)")
        .unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_shape"));
    db.execute_batch("DROP TABLE unsupported").unwrap();
    db.execute_batch("CREATE VIEW unapproved_view AS SELECT 1")
        .unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_shape"));
    db.execute_batch("DROP VIEW unapproved_view").unwrap();
    db.execute_batch("CREATE TRIGGER unapproved_trigger AFTER INSERT ON graph_nodes BEGIN SELECT RAISE(FAIL, 'FORGED'); END;")
        .unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_shape"));
    db.execute_batch("DROP TRIGGER unapproved_trigger").unwrap();
    assert_eq!(inspect(), ("unknown", "recent_open"));
    // Any earlier schema is collectable whatever its layout, root or age (#253).
    for version in [1, 7, 9, 13, 14, 15] {
        db.pragma_update(None, "user_version", version).unwrap();
        assert_eq!(inspect(), ("eligible", "old_schema"), "v{version}");
    }
    db.pragma_update(None, "user_version", 0).unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_schema"));
    db.pragma_update(None, "user_version", 22).unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_schema"));
    db.pragma_update(None, "user_version", 21).unwrap();
    assert_eq!(inspect(), ("unknown", "recent_open"));
    db.execute("UPDATE index_metadata SET root_inode='1'", [])
        .unwrap();
    assert_eq!(inspect(), ("eligible", "root_replaced"));
}

#[test]
fn complete_schema21_index_remains_gc_eligible() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(common::open_store(temp.path(), &work).unwrap());
    let db = rusqlite::Connection::open(roots.index_db(&identity)).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        21
    );
    let now = 1_800_000_000_i64;
    db.execute(
        "UPDATE index_metadata SET last_opened_at=?1",
        [now - 30 * 86_400],
    )
    .unwrap();
    drop(db);
    let entry = roots.gc_report_at(now).unwrap().derived.remove(0);
    assert_eq!((entry.status, entry.reason), ("eligible", "age_30_days"));
}

#[test]
fn saved_anchor_raw_bytes_survive_edits() {
    use trellis::{
        model::{Annotation, AnnotationRecord},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let raw = serde_json::value::RawValue::from_string(
        r#"{ "syntaxId":"sid:v1:0123456789abcdef0123456789abcdef", "document":{"sourceSetId":"set","language":"rust","path":"src/lib.rs"}, "capturedRevisionId":"rev", "headerHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "siblingGroupHash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "siblingCount":1, "identicalHeaderCount":1 }"#.into(),
    ).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    records
        .put_annotation_record(
            &AnnotationRecord::from_base(
                Annotation {
                    id: "note".into(),
                    node_id: "sid:v1:0123456789abcdef0123456789abcdef".into(),
                    body: "first".into(),
                },
                Some("Title".into()),
                Some(raw),
            ),
            false,
        )
        .unwrap();
    let before = records
        .annotation_record("note")
        .unwrap()
        .unwrap()
        .anchor
        .unwrap()
        .get()
        .to_owned();
    let edited = AnnotationRecord::from_base(
        Annotation {
            id: "note".into(),
            node_id: "sid:v1:0123456789abcdef0123456789abcdef".into(),
            body: "edited body".into(),
        },
        Some("Edited title".into()),
        None,
    );
    let response = records
        .update_annotation_record(&edited, false, || {
            panic!("existing edit must not recapture")
        })
        .unwrap();
    assert_eq!(response.anchor.as_ref().unwrap().get(), before);
    assert_eq!(response.title.as_deref(), Some("Edited title"));
    assert_eq!(response.body, "edited body");
    let after = records.annotation_record("note").unwrap().unwrap();
    assert_eq!(after.anchor.unwrap().get(), before);
    assert_eq!(after.title.as_deref(), Some("Edited title"));
    assert_eq!(after.body, "edited body");
}

#[test]
fn saved_anchor_rejects_target_replacement() {
    use trellis::{model::Annotation, store::topology::DurableRecords};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    records
        .put_annotation(&Annotation {
            id: "note".into(),
            node_id: "old".into(),
            body: "first".into(),
        })
        .unwrap();
    let error = records
        .put_annotation(&Annotation {
            id: "note".into(),
            node_id: "new".into(),
            body: "second".into(),
        })
        .unwrap_err();
    assert!(error.to_string().contains("target replacement"));
    assert_eq!(records.annotation("note").unwrap().unwrap().node_id, "old");
}

fn assert_expected_storage_busy(error: &anyhow::Error) {
    if error.to_string().starts_with("storage_busy") {
        return;
    }
    match error.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(code, _))
            if code.code == rusqlite::ErrorCode::DatabaseBusy => {}
        _ => panic!("unexpected contention error: {error:#}"),
    }
}

fn test_anchor_raw(target: &str, hash_byte: char) -> Box<serde_json::value::RawValue> {
    let hash: String = std::iter::repeat_n(hash_byte, 64).collect();
    serde_json::value::to_raw_value(&trellis::model::DurableAnchor {
        syntax_id: target.into(),
        document: trellis::native_evidence::DocumentKey {
            source_set_id: "set".into(),
            language: "rust".into(),
            path: "src/lib.rs".into(),
        },
        captured_revision_id: "revision".into(),
        header_hash: hash.clone(),
        sibling_group_hash: hash,
        sibling_count: 1,
        identical_header_count: 1,
    })
    .unwrap()
}

#[test]
fn malformed_anchor_documents_fail_before_persistence() {
    use trellis::{
        model::{Annotation, AnnotationRecord},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    for (language, path) in [
        ("typescript", "src/lib.rs"),
        ("rust", "/src/lib.rs"),
        ("rust", "src\\lib.rs"),
        ("rust", "src//lib.rs"),
        ("rust", "src/./lib.rs"),
        ("rust", "src/../lib.rs"),
    ] {
        let mut value: serde_json::Value = serde_json::from_str(
            test_anchor_raw("sid:v1:0123456789abcdef0123456789abcdef", 'a').get(),
        )
        .unwrap();
        value["document"]["language"] = language.into();
        value["document"]["path"] = path.into();
        let raw = serde_json::value::to_raw_value(&value).unwrap();
        let record = AnnotationRecord::from_base(
            Annotation {
                id: "bad".into(),
                node_id: "node".into(),
                body: "body".into(),
            },
            None,
            Some(raw),
        );
        assert!(
            records.put_annotation_record(&record, false).is_err(),
            "accepted {language}:{path}"
        );
    }
    assert!(!roots.record_db(&identity).exists());
}

#[test]
fn view_anchor_raw_bytes_survive_edits() {
    use std::collections::BTreeMap;
    use trellis::{
        model::{SavedView, SavedViewRecord, ViewQuery},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let target = "sid:v1:0123456789abcdef0123456789abcdef";
    let records = DurableRecords::new(&roots, &identity);
    let view = SavedView {
        id: "view".into(),
        title: "First".into(),
        query: ViewQuery {
            seed: target.into(),
            depth: 1,
            max_nodes: 40,
            max_calls: 200,
            include_callbacks: false,
            exclude_paths: vec![],
        },
        pins: BTreeMap::new(),
        hidden: vec![],
    };
    let raw = serde_json::value::RawValue::from_string(
        r#"{ "siblingCount":1, "syntaxId":"sid:v1:0123456789abcdef0123456789abcdef", "document": {"path":"src/lib.rs","language":"rust","sourceSetId":"set"}, "capturedRevisionId":"revision", "headerHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "siblingGroupHash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "identicalHeaderCount":1 }"#.into(),
    ).unwrap();
    records
        .put_view_record(&SavedViewRecord::from_base(view.clone(), Some(raw)))
        .unwrap();
    let before = records
        .view_record("view")
        .unwrap()
        .unwrap()
        .anchor
        .unwrap()
        .get()
        .to_owned();
    let mut edited = view;
    edited.title = "Edited".into();
    records.put_view(&edited).unwrap();
    let after = records.view_record("view").unwrap().unwrap();
    assert_eq!(after.anchor.unwrap().get(), before);
    assert_eq!(after.title, "Edited");
}

#[test]
fn existing_saved_writers_use_exclusive_lock_and_orphan_sidecars_refuse() {
    use trellis::{
        model::{SavedView, SavedViewRecord},
        store::topology::{DurableRecords, UseGuard},
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    let target = "sid:v1:0123456789abcdef0123456789abcdef";
    let view: SavedView = serde_json::from_value(serde_json::json!({
        "id": "exclusive-record", "title": "initial", "query": {"seed": target}
    }))
    .unwrap();
    let initial = SavedViewRecord::from_base(view, None);
    let first = records
        .update_view_record(&initial, || Ok(test_anchor_raw(target, 'a')))
        .unwrap();
    let first_anchor = first.anchor.as_ref().unwrap().get().to_owned();
    let mut edited = initial.clone();
    edited.title = "edited".into();
    let db = roots.record_db(&identity);
    let before = fs::read(&db).unwrap();
    let lock = roots.record_use_lock(&identity);
    let guard = UseGuard::acquire_existing(&lock, true, true).unwrap();
    let save_error = records
        .update_view_record(&edited, || panic!("busy writer must not capture an anchor"))
        .unwrap_err();
    assert_eq!(save_error.to_string(), "storage_busy");
    let delete_error = records.delete_view("exclusive-record").unwrap_err();
    assert_eq!(delete_error.to_string(), "storage_busy");
    assert_eq!(
        fs::read(&db).unwrap(),
        before,
        "busy writers must not touch record bytes"
    );
    drop(guard);
    let preserved = records.view_record("exclusive-record").unwrap().unwrap();
    assert_eq!(preserved.anchor.unwrap().get(), first_anchor);

    let updated = records
        .update_view_record(&edited, || panic!("existing edit must keep first anchor"))
        .unwrap();
    assert_eq!(updated.title, "edited");
    assert_eq!(updated.anchor.unwrap().get(), first_anchor);
    assert!(records.delete_view("exclusive-record").unwrap());
    assert!(records.view_record("exclusive-record").unwrap().is_none());
    let recreated = records
        .update_view_record(&initial, || Ok(test_anchor_raw(target, 'b')))
        .unwrap();
    let second_anchor = recreated.anchor.unwrap().get().to_owned();
    assert_ne!(
        second_anchor, first_anchor,
        "delete followed by save must recapture"
    );

    for suffix in ["-journal", "-wal", "-shm"] {
        let sidecar = db.with_file_name(format!("workspace.db{suffix}"));
        fs::write(&sidecar, b"orphaned").unwrap();
        let before = fs::read(&db).unwrap();
        let save_error = records
            .update_view_record(&edited, || panic!("orphan sidecar must not capture"))
            .unwrap_err();
        assert_eq!(
            save_error.to_string(),
            "incomplete_record: recovery required"
        );
        let delete_error = records.delete_view("exclusive-record").unwrap_err();
        assert_eq!(
            delete_error.to_string(),
            "incomplete_record: recovery required"
        );
        assert_eq!(fs::read(&db).unwrap(), before);
        assert_eq!(fs::read(&sidecar).unwrap().as_slice(), b"orphaned");
        fs::remove_file(sidecar).unwrap();
    }
    let preserved = records.view_record("exclusive-record").unwrap().unwrap();
    assert_eq!(preserved.anchor.unwrap().get(), second_anchor);
}

#[test]
fn saved_anchor_atomic_first_save_and_delete_edit_races() {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Barrier},
        thread,
    };
    use trellis::{
        model::{Annotation, AnnotationRecord, SavedView, SavedViewRecord, ViewQuery},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let target = "sid:v1:0123456789abcdef0123456789abcdef";
    let make_view = |title: char| {
        SavedViewRecord::from_base(
            SavedView {
                id: "view-race".into(),
                title: title.to_string(),
                query: ViewQuery {
                    seed: target.into(),
                    depth: 1,
                    max_nodes: 40,
                    max_calls: 200,
                    include_callbacks: false,
                    exclude_paths: vec![],
                },
                pins: BTreeMap::new(),
                hidden: vec![],
            },
            None,
        )
    };

    // Whole-record creation intentionally uses a nonblocking exclusive use lock.
    // One contender wins; retry the typed busy loser only after the winner released it.
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = vec![];
    for hash in ['a', 'b'] {
        let roots = roots.clone();
        let work = work.clone();
        let barrier = barrier.clone();
        let record = make_view(hash);
        workers.push(thread::spawn(move || {
            let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
            barrier.wait();
            let result = DurableRecords::new(&roots, &identity)
                .update_view_record(&record, || Ok(test_anchor_raw(target, hash)));
            (hash, result)
        }));
    }
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let winner = outcomes
        .iter()
        .find_map(|(_, result)| result.as_ref().ok())
        .expect("one creator must win");
    let winner_raw = winner.anchor.as_ref().unwrap().get().to_owned();
    let losers: Vec<_> = outcomes
        .iter()
        .filter(|(_, result)| result.is_err())
        .collect();
    assert!(losers.len() <= 1, "only the nonblocking creator may lose");
    for (_, result) in outcomes.iter().filter(|(_, result)| result.is_ok()) {
        assert_eq!(
            result.as_ref().unwrap().anchor.as_ref().unwrap().get(),
            winner_raw
        );
    }
    if let Some((loser_hash, loser)) = losers.first() {
        assert_expected_storage_busy(loser.as_ref().unwrap_err());
        let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        let retried = DurableRecords::new(&roots, &identity)
            .update_view_record(&make_view(*loser_hash), || {
                Ok(test_anchor_raw(target, *loser_hash))
            })
            .unwrap();
        assert_eq!(retried.anchor.as_ref().unwrap().get(), winner_raw);
    }

    // Once the durable DB exists, same-ID note contenders either serialize in SQLite
    // or report typed busy. Retry any loser after both contenders have completed.
    let make_note = |body: char| {
        AnnotationRecord::from_base(
            Annotation {
                id: "note-race".into(),
                node_id: target.into(),
                body: body.to_string(),
            },
            None,
            None,
        )
    };
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = vec![];
    for hash in ['c', 'd'] {
        let roots = roots.clone();
        let work = work.clone();
        let barrier = barrier.clone();
        let record = make_note(hash);
        workers.push(thread::spawn(move || {
            let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
            barrier.wait();
            let result = DurableRecords::new(&roots, &identity).update_annotation_record(
                &record,
                false,
                || Ok(test_anchor_raw(target, hash)),
            );
            (hash, result)
        }));
    }
    let outcomes: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let winner = outcomes
        .iter()
        .find_map(|(_, result)| result.as_ref().ok())
        .expect("one note writer must win");
    let note_winner_raw = winner.anchor.as_ref().unwrap().get().to_owned();
    for (hash, result) in outcomes {
        match result {
            Ok(record) => assert_eq!(record.anchor.unwrap().get(), note_winner_raw),
            Err(error) => {
                assert_expected_storage_busy(&error);
                let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
                let retried = DurableRecords::new(&roots, &identity)
                    .update_annotation_record(&make_note(hash), false, || {
                        Ok(test_anchor_raw(target, hash))
                    })
                    .unwrap();
                assert_eq!(retried.anchor.unwrap().get(), note_winner_raw);
            }
        }
    }

    // Delete/edit contention may expose the same documented typed busy result.
    // Complete the loser only after the winner releases the transaction and prove
    // that every successful edit response carries a real immutable anchor.
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    let existing = records.view_record("view-race").unwrap().unwrap();
    let old_raw = existing.anchor.as_ref().unwrap().get().to_owned();
    let edit = SavedViewRecord {
        title: "edit".into(),
        anchor: None,
        ..existing
    };
    let barrier = Arc::new(Barrier::new(2));
    let edit_roots = roots.clone();
    let edit_work = work.clone();
    let edit_barrier = barrier.clone();
    let edit_copy = edit.clone();
    let editor = thread::spawn(move || {
        let identity = WorkspaceIdentity::discover(Some(&edit_work), &edit_work).unwrap();
        edit_barrier.wait();
        DurableRecords::new(&edit_roots, &identity)
            .update_view_record(&edit_copy, || Ok(test_anchor_raw(target, 'e')))
    });
    let delete_roots = roots.clone();
    let delete_work = work.clone();
    let delete_barrier = barrier.clone();
    let deleter = thread::spawn(move || {
        let identity = WorkspaceIdentity::discover(Some(&delete_work), &delete_work).unwrap();
        delete_barrier.wait();
        DurableRecords::new(&delete_roots, &identity).delete_view("view-race")
    });
    let replacement_raw = test_anchor_raw(target, 'e').get().to_owned();
    let edit_result = editor.join().unwrap();
    let delete_result = deleter.join().unwrap();
    let edit_was_busy = edit_result.is_err();
    let edit_response = match edit_result {
        Ok(record) => record,
        Err(error) => {
            assert_expected_storage_busy(&error);
            records
                .update_view_record(&edit, || Ok(test_anchor_raw(target, 'e')))
                .unwrap()
        }
    };
    let delete_was_busy = delete_result.is_err();
    match delete_result {
        Ok(deleted) => assert!(deleted),
        Err(error) => {
            assert_expected_storage_busy(&error);
            assert!(records.delete_view("view-race").unwrap());
        }
    }
    let response_raw = edit_response.anchor.as_ref().unwrap().get();
    assert!(response_raw == old_raw || response_raw == replacement_raw);
    assert!(edit_response.typed_anchor().unwrap().is_some());
    let final_record = records.view_record("view-race").unwrap();
    match (edit_was_busy, delete_was_busy, final_record) {
        // The edit retry runs first; the delete retry then removes that exact row.
        (_, true, None) => assert_eq!(response_raw, old_raw),
        // The completed delete ran first; the edit retry is a fresh server capture.
        (true, false, Some(final_record)) => {
            assert_eq!(response_raw, replacement_raw);
            assert_eq!(final_record.anchor.unwrap().get(), replacement_raw);
        }
        // With no retry, final presence proves delete-before-edit.
        (false, false, Some(final_record)) => {
            assert_eq!(response_raw, replacement_raw);
            assert_eq!(final_record.anchor.unwrap().get(), replacement_raw);
        }
        // With no retry, final absence proves edit-before-delete.
        (false, false, None) => assert_eq!(response_raw, old_raw),
        chronology => panic!("impossible view contention chronology: {chronology:?}"),
    }

    let existing = records.annotation_record("note-race").unwrap().unwrap();
    let old_raw = existing.anchor.as_ref().unwrap().get().to_owned();
    let edit = AnnotationRecord {
        body: "edit".into(),
        anchor: None,
        ..existing
    };
    let barrier = Arc::new(Barrier::new(2));
    let edit_roots = roots.clone();
    let edit_work = work.clone();
    let edit_barrier = barrier.clone();
    let edit_copy = edit.clone();
    let editor = thread::spawn(move || {
        let identity = WorkspaceIdentity::discover(Some(&edit_work), &edit_work).unwrap();
        edit_barrier.wait();
        DurableRecords::new(&edit_roots, &identity).update_annotation_record(
            &edit_copy,
            false,
            || Ok(test_anchor_raw(target, 'f')),
        )
    });
    let delete_roots = roots.clone();
    let delete_work = work.clone();
    let delete_barrier = barrier.clone();
    let deleter = thread::spawn(move || {
        let identity = WorkspaceIdentity::discover(Some(&delete_work), &delete_work).unwrap();
        delete_barrier.wait();
        DurableRecords::new(&delete_roots, &identity).delete_annotation("note-race")
    });
    let replacement_raw = test_anchor_raw(target, 'f').get().to_owned();
    let edit_result = editor.join().unwrap();
    let delete_result = deleter.join().unwrap();
    let edit_was_busy = edit_result.is_err();
    let edit_response = match edit_result {
        Ok(record) => record,
        Err(error) => {
            assert_expected_storage_busy(&error);
            records
                .update_annotation_record(&edit, false, || Ok(test_anchor_raw(target, 'f')))
                .unwrap()
        }
    };
    let delete_was_busy = delete_result.is_err();
    match delete_result {
        Ok(deleted) => assert!(deleted),
        Err(error) => {
            assert_expected_storage_busy(&error);
            assert!(records.delete_annotation("note-race").unwrap());
        }
    }
    let response_raw = edit_response.anchor.as_ref().unwrap().get();
    assert!(response_raw == old_raw || response_raw == replacement_raw);
    assert!(edit_response.typed_anchor().unwrap().is_some());
    let final_record = records.annotation_record("note-race").unwrap();
    match (edit_was_busy, delete_was_busy, final_record) {
        // The edit retry runs first; the delete retry then removes that exact row.
        (_, true, None) => assert_eq!(response_raw, old_raw),
        // The completed delete ran first; the edit retry is a fresh server capture.
        (true, false, Some(final_record)) => {
            assert_eq!(response_raw, replacement_raw);
            assert_eq!(final_record.anchor.unwrap().get(), replacement_raw);
        }
        // With no retry, final presence proves delete-before-edit.
        (false, false, Some(final_record)) => {
            assert_eq!(response_raw, replacement_raw);
            assert_eq!(final_record.anchor.unwrap().get(), replacement_raw);
        }
        // With no retry, final absence proves edit-before-delete.
        (false, false, None) => assert_eq!(response_raw, old_raw),
        chronology => panic!("impossible view contention chronology: {chronology:?}"),
    }
}

#[test]
fn server_updates_recapture_after_confirmed_delete_and_reject_stale_raw_input() {
    use std::{
        collections::BTreeMap,
        sync::atomic::{AtomicBool, Ordering},
    };
    use trellis::{
        model::{Annotation, AnnotationRecord, SavedView, SavedViewRecord, ViewQuery},
        store::topology::DurableRecords,
    };
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let records = DurableRecords::new(&roots, &identity);
    let target = "sid:v1:0123456789abcdef0123456789abcdef";

    let stale_view = SavedViewRecord::from_base(
        SavedView {
            id: "view-recreate".into(),
            title: "old".into(),
            query: ViewQuery {
                seed: target.into(),
                depth: 1,
                max_nodes: 40,
                max_calls: 200,
                include_callbacks: false,
                exclude_paths: vec![],
            },
            pins: BTreeMap::new(),
            hidden: vec![],
        },
        Some(test_anchor_raw(target, 'a')),
    );
    records.put_view_record(&stale_view).unwrap();
    assert!(records.delete_view("view-recreate").unwrap());
    assert!(records.view_record("view-recreate").unwrap().is_none());
    let view_capture_called = AtomicBool::new(false);
    let recreated_view = records
        .update_view_record(&stale_view, || {
            assert!(!view_capture_called.swap(true, Ordering::SeqCst));
            Ok(test_anchor_raw(target, 'e'))
        })
        .unwrap();
    assert!(view_capture_called.load(Ordering::SeqCst));
    assert_eq!(
        recreated_view.anchor.as_ref().unwrap().get(),
        test_anchor_raw(target, 'e').get()
    );
    assert_eq!(
        records
            .view_record("view-recreate")
            .unwrap()
            .unwrap()
            .anchor
            .unwrap()
            .get(),
        test_anchor_raw(target, 'e').get()
    );

    let stale_note = AnnotationRecord::from_base(
        Annotation {
            id: "note-recreate".into(),
            node_id: target.into(),
            body: "old".into(),
        },
        Some("Title".into()),
        Some(test_anchor_raw(target, 'b')),
    );
    records.put_annotation_record(&stale_note, false).unwrap();
    assert!(records.delete_annotation("note-recreate").unwrap());
    assert!(
        records
            .annotation_record("note-recreate")
            .unwrap()
            .is_none()
    );
    let note_capture_called = AtomicBool::new(false);
    let recreated_note = records
        .update_annotation_record(&stale_note, false, || {
            assert!(!note_capture_called.swap(true, Ordering::SeqCst));
            Ok(test_anchor_raw(target, 'f'))
        })
        .unwrap();
    assert!(note_capture_called.load(Ordering::SeqCst));
    assert_eq!(
        recreated_note.anchor.as_ref().unwrap().get(),
        test_anchor_raw(target, 'f').get()
    );
    assert_eq!(
        records
            .annotation_record("note-recreate")
            .unwrap()
            .unwrap()
            .anchor
            .unwrap()
            .get(),
        test_anchor_raw(target, 'f').get()
    );
}

#[cfg(feature = "test-support")]
#[test]
fn gc_report_accepts_current_schema21_but_rejects_bad_inventory() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let store = common::open_store(temp.path(), &work).unwrap();
    let writer = store.publication_for_tests().unwrap();
    drop(writer);
    drop(store);
    let db = rusqlite::Connection::open(roots.index_db(&identity)).unwrap();
    let now = 1_800_000_000_i64;
    db.execute("UPDATE index_metadata SET last_opened_at=?1", [now])
        .unwrap();
    let inspect = || roots.gc_report_at(now).unwrap().derived.remove(0);
    let good = inspect();
    assert_eq!((good.status, good.reason), ("unknown", "recent_open"));
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    db.execute(
        "INSERT INTO native_revision_supersessions(revision_id,superseded_at,state) VALUES('missing',0,'retained')",
        [],
    )
    .unwrap();
    let bad = inspect();
    assert_eq!(
        (bad.status, bad.reason),
        ("unknown", "invalid_supersession_inventory")
    );
    db.execute("DELETE FROM native_revision_supersessions", [])
        .unwrap();
    assert_eq!(inspect().reason, "recent_open");
}

#[cfg(feature = "test-support")]
#[test]
fn automatic_gc_deletes_old_schema_indexes_unless_in_use_or_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, roots) = common::fixture();
    let now = 1_800_000_000_i64;
    // Old layouts need no particular shape, metadata, root or age (#253).
    let mut old = Vec::new();
    for (name, version) in [("v7", 7_i64), ("v12", 12), ("v13-in-use", 13)] {
        let workspace = temp.path().join(name);
        fs::create_dir(&workspace).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap();
        // Provision the fixture without opening a current Store.
        drop(roots.index_use(&identity).unwrap());
        let path = roots.index_db(&identity);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE anything(x)").unwrap();
        db.pragma_update(None, "user_version", version).unwrap();
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        if version == 7 {
            // Its request queue, accepted rows or not, is deleted with it.
            let queue = path.with_file_name("requests.db");
            let db = rusqlite::Connection::open(&queue).unwrap();
            db.execute_batch(
                "CREATE TABLE requests(id TEXT); INSERT INTO requests VALUES('queued');",
            )
            .unwrap();
            drop(db);
            fs::set_permissions(&queue, fs::Permissions::from_mode(0o600)).unwrap();
        }
        old.push(identity);
    }
    let unreadable_root = temp.path().join("unreadable");
    fs::create_dir(&unreadable_root).unwrap();
    let unreadable = WorkspaceIdentity::discover(Some(&unreadable_root), &unreadable_root).unwrap();
    drop(roots.index_use(&unreadable).unwrap());
    fs::write(roots.index_db(&unreadable), b"not a sqlite database").unwrap();
    fs::set_permissions(
        roots.index_db(&unreadable),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let current_root = temp.path().join("current-gc");
    fs::create_dir(&current_root).unwrap();
    drop(common::open_store(temp.path(), &current_root).unwrap());
    let current = WorkspaceIdentity::discover(Some(&current_root), &current_root).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    for identity in &old {
        let entry = roots
            .gc_report_at(now)
            .unwrap()
            .derived
            .into_iter()
            .find(|entry| entry.root_key == identity.root_key)
            .unwrap();
        assert_eq!((entry.status, entry.reason), ("eligible", "old_schema"));
    }
    let in_use = &old[2];
    let held = trellis::store::topology::UseGuard::acquire_existing(
        &roots.index_use_lock(in_use),
        false,
        true,
    )
    .unwrap();
    let unreadable_bytes = fs::read(roots.index_db(&unreadable)).unwrap();
    assert_eq!(roots.automatic_gc_at(&current, &writer, now).unwrap(), 2);
    for identity in &old[..2] {
        assert!(!roots.index_dir(identity).exists(), "{}", identity.root_key);
    }
    assert!(roots.index_db(in_use).exists(), "an index in use survives");
    assert_eq!(
        fs::read(roots.index_db(&unreadable)).unwrap(),
        unreadable_bytes,
        "an unreadable index stays untouched"
    );
    assert!(roots.index_db(&current).exists());
    drop(held);
}

#[cfg(feature = "test-support")]
#[test]
fn automatic_gc_only_eligible_exact_private_indexes() {
    let (temp, roots) = common::fixture();
    let old = root(temp.path());
    let old_id = WorkspaceIdentity::discover(Some(&old), &old).unwrap();
    drop(common::open_store(temp.path(), &old).unwrap());
    drop(
        roots
            .writer(std::sync::Arc::new(
                old_id.verified_clone_for_tests().unwrap(),
            ))
            .unwrap(),
    );
    let recent = temp.path().join("recent");
    fs::create_dir(&recent).unwrap();
    let recent_id = WorkspaceIdentity::discover(Some(&recent), &recent).unwrap();
    drop(common::open_store(temp.path(), &recent).unwrap());
    drop(
        roots
            .writer(std::sync::Arc::new(
                recent_id.verified_clone_for_tests().unwrap(),
            ))
            .unwrap(),
    );
    let current = temp.path().join("current");
    fs::create_dir(&current).unwrap();
    let current_store = common::open_store(temp.path(), &current).unwrap();
    let current_id = WorkspaceIdentity::discover(Some(&current), &current).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current_id.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let now = 1_800_000_000_i64;
    rusqlite::Connection::open(roots.index_db(&old_id))
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 30 * 24 * 60 * 60],
        )
        .unwrap();
    rusqlite::Connection::open(roots.index_db(&recent_id))
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 29 * 24 * 60 * 60],
        )
        .unwrap();
    let durable_before = gc_manifest(&roots.data);
    assert_eq!(current_store.automatic_gc_at(&writer, now).unwrap(), 1);
    let stamp = roots.cache.join("gc-last-run");
    assert_eq!(fs::read_to_string(&stamp).unwrap(), format!("{now}\n"));
    use std::os::unix::fs::MetadataExt;
    assert_eq!(fs::symlink_metadata(&stamp).unwrap().mode() & 0o777, 0o600);
    assert!(!roots.index_dir(&old_id).exists());
    assert!(!roots.index_use_lock(&old_id).exists());
    assert!(roots.index_dir(&recent_id).exists(), "recent root remains");
    assert!(
        roots.index_dir(&current_id).exists(),
        "current writer root remains"
    );
    assert_eq!(durable_before, gc_manifest(&roots.data));
    assert_eq!(
        current_store
            .automatic_gc_at(&writer, now + 86_399)
            .unwrap(),
        0
    );
    assert!(roots.index_dir(&recent_id).exists());
    assert_eq!(
        current_store
            .automatic_gc_at(&writer, now + 86_400)
            .unwrap(),
        1
    );
    assert!(
        !roots.index_dir(&recent_id).exists(),
        "30-day boundary permits deletion"
    );
}

#[cfg(feature = "test-support")]
#[test]
fn automatic_gc_rejects_unknown_shapes_busy_and_hot_files() {
    let (temp, roots) = common::fixture();
    let current = root(temp.path());
    let store = common::open_store(temp.path(), &current).unwrap();
    let current_id = WorkspaceIdentity::discover(Some(&current), &current).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current_id.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let now = 1_800_000_000_i64;
    let mut held_locks = Vec::new();
    for (n, case) in [
        "unknown",
        "table",
        "trigger",
        "index",
        "busy",
        "hot",
        "queue",
        "hardlink",
        "symlink",
        "unsafe_mode",
        "bad_generation",
    ]
    .iter()
    .enumerate()
    {
        let work = temp.path().join(format!("candidate-{n}"));
        fs::create_dir(&work).unwrap();
        let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
        let candidate = common::open_store(temp.path(), &work).unwrap();
        if *case == "queue" {
            candidate
                .enqueue_request(&trellis::indexer::IndexOptions::new(work.clone()), None)
                .unwrap();
        }
        drop(candidate);
        drop(
            roots
                .writer(std::sync::Arc::new(id.verified_clone_for_tests().unwrap()))
                .unwrap(),
        );
        let index = roots.index_db(&id);
        rusqlite::Connection::open(&index)
            .unwrap()
            .execute(
                "UPDATE index_metadata SET last_opened_at=?1",
                [now - 31 * 24 * 60 * 60],
            )
            .unwrap();
        match *case {
            "unknown" => {
                rusqlite::Connection::open(&index)
                    .unwrap()
                    .execute_batch("CREATE VIEW unsafe_view AS SELECT 1")
                    .unwrap();
            }
            "table" | "trigger" | "index" => {
                let ddl = match *case {
                    "table" => "CREATE TABLE unsafe_table(id INTEGER)",
                    "trigger" => {
                        "CREATE TRIGGER unsafe_trigger AFTER INSERT ON graph_nodes BEGIN SELECT 1; END"
                    }
                    _ => "CREATE INDEX unsafe_index ON index_metadata(root_inode)",
                };
                rusqlite::Connection::open(&index)
                    .unwrap()
                    .execute_batch(ddl)
                    .unwrap();
            }
            "hot" => {
                fs::write(index.with_file_name("index.db-journal"), b"hot").unwrap();
            }
            "queue" => {
                rusqlite::Connection::open(roots.requests_db(&id))
                    .unwrap()
                    .execute_batch("CREATE VIEW unsafe_queue AS SELECT 1")
                    .unwrap();
            }
            "bad_generation" => {
                rusqlite::Connection::open(&index)
                    .unwrap()
                    .execute(
                        "UPDATE index_metadata SET index_generation='not-a-uuid'",
                        [],
                    )
                    .unwrap();
            }
            "hardlink" => {
                fs::hard_link(&index, temp.path().join("protected-extra-link")).unwrap();
            }
            "symlink" => {
                let moved = temp.path().join("protected-moved-index");
                fs::rename(&index, &moved).unwrap();
                std::os::unix::fs::symlink(&moved, &index).unwrap();
            }
            "unsafe_mode" => {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => {}
        }
        if *case == "busy" {
            held_locks.push(
                UseGuard::acquire_existing(&roots.index_use_lock(&id), false, false).unwrap(),
            );
        }
        assert_eq!(
            store
                .automatic_gc_at(&writer, now + n as i64 * 86_400)
                .unwrap(),
            0,
            "{case}"
        );
        assert!(roots.index_dir(&id).exists(), "{case} must not be deleted");
        if *case == "unsafe_mode" {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&index).unwrap().permissions().mode() & 0o777,
                0o644
            );
            fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).unwrap();
            held_locks.push(
                UseGuard::acquire_existing(&roots.index_use_lock(&id), false, false).unwrap(),
            );
        }
    }
}

#[cfg(feature = "test-support")]
#[test]
fn automatic_gc_missing_or_replaced_root_is_alternate_eligibility() {
    let (temp, roots) = common::fixture();
    let current = root(temp.path());
    let store = common::open_store(temp.path(), &current).unwrap();
    let current_id = WorkspaceIdentity::discover(Some(&current), &current).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current_id.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let mut candidates = Vec::new();
    for case in ["missing", "replaced"] {
        let path = temp.path().join(case);
        fs::create_dir(&path).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&path), &path).unwrap();
        drop(common::open_store(temp.path(), &path).unwrap());
        drop(
            roots
                .writer(std::sync::Arc::new(
                    identity.verified_clone_for_tests().unwrap(),
                ))
                .unwrap(),
        );
        // Keep recent last_opened_at: lost root is an independent eligible case.
        fs::rename(&path, temp.path().join(format!("{case}-moved"))).unwrap();
        if case == "replaced" {
            fs::create_dir(&path).unwrap();
        }
        candidates.push(identity);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(store.automatic_gc_at(&writer, now).unwrap(), 2);
    for identity in candidates {
        assert!(!roots.index_dir(&identity).exists());
        assert!(!roots.index_use_lock(&identity).exists());
    }
}

#[cfg(feature = "test-support")]
#[test]
fn automatic_gc_refuses_malformed_or_future_attempt_stamp() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let store = common::open_store(temp.path(), &work).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            identity.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let now = 1_800_000_000_i64;
    assert_eq!(store.automatic_gc_at(&writer, now).unwrap(), 0);
    let stamp = roots.cache.join("gc-last-run");
    let original = fs::read(&stamp).unwrap();
    assert!(store.automatic_gc_at(&writer, now - 1).is_err());
    assert_eq!(fs::read(&stamp).unwrap(), original);
    fs::write(&stamp, b"broken\n").unwrap();
    assert!(store.automatic_gc_at(&writer, now + 86_400).is_err());
    assert_eq!(fs::read(&stamp).unwrap(), b"broken\n");
    assert!(roots.index_dir(&identity).exists());
}

#[cfg(feature = "test-support")]
#[test]
fn attempt2_gc_rechecks_current_root_at_final_unlink() {
    use trellis::store::topology::GcStage;
    let (temp, roots) = common::fixture();
    let current = root(temp.path());
    let store = common::open_store(temp.path(), &current).unwrap();
    let current_id = WorkspaceIdentity::discover(Some(&current), &current).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current_id.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let old = temp.path().join("old");
    fs::create_dir(&old).unwrap();
    let old_id = WorkspaceIdentity::discover(Some(&old), &old).unwrap();
    drop(common::open_store(temp.path(), &old).unwrap());
    drop(
        roots
            .writer(std::sync::Arc::new(
                old_id.verified_clone_for_tests().unwrap(),
            ))
            .unwrap(),
    );
    let now = 1_800_000_000_i64;
    let index = roots.index_db(&old_id);
    rusqlite::Connection::open(&index)
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 24 * 60 * 60],
        )
        .unwrap();
    let before = fs::read(&index).unwrap();
    let candidate_before = gc_manifest(&roots.index_dir(&old_id));
    let mut calls = 0;
    let result = roots.automatic_gc_at_with_hook(&current_id, &writer, now, &mut |stage| {
        if stage == GcStage::BeforeCandidateUnlink {
            calls += 1;
            fs::rename(&current, temp.path().join("moved-current"))?;
        }
        Ok(())
    });
    assert_eq!(calls, 1, "the barrier must reach the final unlink boundary");
    assert!(result.is_err(), "lost current root must abort deletion");
    assert_eq!(fs::read(&index).unwrap(), before);
    assert_eq!(
        gc_manifest(&roots.index_dir(&old_id)),
        candidate_before,
        "all candidate pathnames and bytes must remain untouched"
    );
    assert!(roots.index_use_lock(&old_id).exists());
    drop(store);
}

#[cfg(feature = "test-support")]
#[test]
fn attempt2_gc_existing_empty_stamp_must_not_scan() {
    let (temp, roots) = common::fixture();
    let current = root(temp.path());
    drop(common::open_store(temp.path(), &current).unwrap());
    let current_id = WorkspaceIdentity::discover(Some(&current), &current).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(
            current_id.verified_clone_for_tests().unwrap(),
        ))
        .unwrap();
    let old = temp.path().join("old");
    fs::create_dir(&old).unwrap();
    let old_id = WorkspaceIdentity::discover(Some(&old), &old).unwrap();
    drop(common::open_store(temp.path(), &old).unwrap());
    drop(
        roots
            .writer(std::sync::Arc::new(
                old_id.verified_clone_for_tests().unwrap(),
            ))
            .unwrap(),
    );
    let now = 1_800_000_000_i64;
    let index = roots.index_db(&old_id);
    rusqlite::Connection::open(&index)
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 24 * 60 * 60],
        )
        .unwrap();
    let stamp = roots.cache.join("gc-last-run");
    fs::write(&stamp, b"").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&stamp, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&index).unwrap();
    assert!(roots.automatic_gc_at(&current_id, &writer, now).is_err());
    assert_eq!(fs::read(&index).unwrap(), before);
}

#[cfg(feature = "test-support")]
#[test]
fn attempt2_gc_fault_before_stamp_rename_keeps_prior_fence() {
    use trellis::store::topology::GcStage;
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    drop(common::open_store(temp.path(), &work).unwrap());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let writer = roots
        .writer(std::sync::Arc::new(id.verified_clone_for_tests().unwrap()))
        .unwrap();
    let now = 1_800_000_000_i64;
    assert_eq!(roots.automatic_gc_at(&id, &writer, now).unwrap(), 0);
    let stamp = roots.cache.join("gc-last-run");
    let before = fs::read(&stamp).unwrap();
    let mut called = false;
    let error = roots.automatic_gc_at_with_hook(&id, &writer, now + 86_400, &mut |stage| {
        if stage == GcStage::BeforeStampRename {
            called = true;
            anyhow::bail!("injected crash before atomic stamp rename");
        }
        Ok(())
    });
    assert!(called);
    assert!(error.is_err());
    assert_eq!(
        fs::read(&stamp).unwrap(),
        before,
        "crash must retain old daily fence"
    );
    assert_eq!(
        roots.automatic_gc_at(&id, &writer, now + 86_399).unwrap(),
        0
    );
}
