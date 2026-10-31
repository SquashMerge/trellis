use crate::common;
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
        .arg(child_test_name("implicit_home_child"))
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
fn git_marker_identity_survives_checkout_move_and_copy() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    common::private(&work.join(".git"));
    let original = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let copied = temp.path().join("copy");
    fs::create_dir(&copied).unwrap();
    common::private(&copied.join(".git"));
    common::private(&copied.join(".git/trellis"));
    fs::copy(
        work.join(".git/trellis/workspace-id"),
        copied.join(".git/trellis/workspace-id"),
    )
    .unwrap();
    let copy = WorkspaceIdentity::discover(Some(&copied), &copied).unwrap();
    assert_eq!(copy.record_id, original.record_id);
    assert_ne!(copy.root_key, original.root_key);
    let moved = temp.path().join("moved");
    fs::rename(&work, &moved).unwrap();
    let moved_id = WorkspaceIdentity::discover(Some(&moved), &moved).unwrap();
    assert_eq!(moved_id.record_id, original.record_id);
    assert_ne!(moved_id.root_key, original.root_key);
    assert!(original.verify().is_err());
    copy.verify().unwrap();
    moved_id.verify().unwrap();
    assert!(
        !roots.data.exists(),
        "identity discovery created retired state"
    );
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
                .arg(child_test_name("marker_child"))
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
        .arg(child_test_name("stale_use_child"))
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
        .arg(child_test_name("shared_holder_child"))
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

#[test]
fn existing_use_lock_unlink_never_recreates() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(roots.index_use(&id).unwrap());
    let lock = roots.index_use_lock(&id);
    assert!(
        UseGuard::acquire_existing_with_hook(&lock, false, false, || {
            fs::remove_file(&lock)?;
            Ok(())
        })
        .is_err()
    );
    assert!(!lock.exists(), "existing-only reopen created a lock");
}

#[test]
fn existing_use_lock_replacement_checks_new_inode_and_holder() {
    use std::io::{BufRead, BufReader};
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let id = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    roots.prepare_index(&id).unwrap();
    let lock = roots.index_use_lock(&id);
    drop(roots.index_use(&id).unwrap());
    let mut child = None;
    let mut reader = None;
    let result = UseGuard::acquire_existing_with_hook(&lock, false, true, || {
        fs::remove_file(&lock)?;
        drop(roots.index_use(&id)?);
        let mut process = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(child_test_name("shared_holder_child"))
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
        31
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
fn gc_age_and_derived_inventory_are_read_only() {
    use trellis::store::topology::classify_index_age;
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
    let empty = roots.gc_report_at(now).unwrap();
    assert!(empty.derived.is_empty());
    assert_eq!(empty.vectors.bytes, 0);
    assert_eq!(empty.vectors.entries, 0);
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
    let manifest_before = gc_manifest(temp.path());
    let report = roots.gc_report_at(now).unwrap();
    assert_eq!(manifest_before, gc_manifest(temp.path()));
    assert_eq!(
        (report.derived[0].status, report.derived[0].reason),
        ("eligible", "age_30_days")
    );
    let held = UseGuard::acquire_existing(&roots.index_use_lock(&identity), false, false).unwrap();
    assert_eq!(roots.gc_report_at(now).unwrap().derived[0].status, "busy");
    drop(held);
    fs::rename(&work, temp.path().join("moved")).unwrap();
    let missing = roots.gc_report_at(now).unwrap();
    assert_eq!(missing.derived[0].reason, "root_missing");
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
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(common::open_store(temp.path(), &work).unwrap());
    let db = roots.index_db(&identity);
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = db.with_file_name(format!("index.db{suffix}"));
        symlink(temp.path().join("absent"), &sidecar).unwrap();
        assert!(fs::symlink_metadata(&sidecar).is_ok());
        assert!(!sidecar.exists(), "fixture must be a dangling link");
        let before = gc_manifest(temp.path());
        let report = roots.gc_report_at(1_800_000_000).unwrap();
        assert_eq!(
            (report.derived[0].status, report.derived[0].reason),
            ("unknown", "metadata_unreadable"),
            "{suffix}"
        );
        assert_eq!(
            gc_manifest(temp.path()),
            before,
            "report wrote with {suffix}"
        );
        fs::remove_file(sidecar).unwrap();
    }
}

#[test]
fn gc_report_sorts_multiple_derived_indexes() {
    let (temp, roots) = common::fixture();
    let mut identities = (0..3)
        .map(|n| {
            let work = temp.path().join(format!("sort-{n}"));
            fs::create_dir(&work).unwrap();
            WorkspaceIdentity::discover(Some(&work), &work).unwrap()
        })
        .collect::<Vec<_>>();
    identities.sort_by(|a, b| b.root_key.cmp(&a.root_key));
    let mut expected_keys: Vec<_> = identities.iter().map(|id| id.root_key.clone()).collect();
    for identity in &identities {
        drop(common::open_store(temp.path(), &identity.root).unwrap());
    }
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    expected_keys.sort();
    assert_eq!(
        report
            .derived
            .iter()
            .map(|entry| entry.root_key.clone())
            .collect::<Vec<_>>(),
        expected_keys
    );
}

#[test]
fn gc_classifies_current_schema_but_refuses_spoofed_shapes() {
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
    for version in [
        1, 7, 9, 13, 14, 15, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    ] {
        db.pragma_update(None, "user_version", version).unwrap();
        assert_eq!(inspect(), ("eligible", "old_schema"), "v{version}");
    }
    db.pragma_update(None, "user_version", 0).unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_schema"));
    db.pragma_update(None, "user_version", 33).unwrap();
    assert_eq!(inspect(), ("unknown", "unknown_index_schema"));
    db.pragma_update(None, "user_version", 32).unwrap();
    assert_eq!(inspect(), ("unknown", "recent_open"));
    db.execute("UPDATE index_metadata SET root_inode='1'", [])
        .unwrap();
    assert_eq!(inspect(), ("eligible", "root_replaced"));
}

#[test]
fn complete_current_schema_index_remains_gc_eligible() {
    let (temp, roots) = common::fixture();
    let work = root(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    drop(common::open_store(temp.path(), &work).unwrap());
    let db = rusqlite::Connection::open(roots.index_db(&identity)).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        31
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

#[cfg(feature = "test-support")]
#[test]
fn gc_report_accepts_current_schema_but_rejects_bad_inventory() {
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

#[test]
fn vector_report_is_machine_wide_and_does_not_create_checkout_state() {
    let (temp, roots) = common::fixture();
    roots.prepare_vectors().unwrap();
    let mut cache = trellis::search_vectors::VectorCacheWriter::open(&roots.vectors_db()).unwrap();
    cache
        .put(&"a".repeat(64), &"b".repeat(64), &[1, -2, 3])
        .unwrap();
    drop(cache);
    let before = gc_manifest(temp.path());
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(report.vectors.entries, 1);
    assert!(report.vectors.bytes > 0);
    assert!(report.derived.is_empty());
    assert_eq!(before, gc_manifest(temp.path()));
    assert!(!roots.cache.join("indexes").exists());
    assert!(!roots.data.exists());
}

/// A child test's libtest name in the grouped test binary, which names this
/// module's tests `<module>::<test>`.
fn child_test_name(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}
