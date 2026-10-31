use std::{fs, os::unix::fs::symlink, path::Path, process::Command};
use trellis::daemon::registry::{CheckoutOptions, CheckoutRegistry, SelectionError};
use trellis::model::MAX_ACTIVE_CHECKOUTS;
use trellis::store::topology::WorkspaceIdentity;

fn git(args: &[&str], dir: &Path) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}
fn identity(root: &Path) -> WorkspaceIdentity {
    WorkspaceIdentity::discover(Some(root), root)
        .unwrap()
        .attach_marker()
        .unwrap()
}

#[test]
fn selected_worktree_is_attributed_and_retained_until_disconnect() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let launch = base.join("launch");
    let selected = base.join("selected");
    fs::create_dir(&launch).unwrap();
    git(&["init", "-q"], &launch);
    git(
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
        &launch,
    );
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "selected",
            selected.to_str().unwrap(),
        ],
        &launch,
    );
    let launch_id = identity(&launch);
    let mut registry = CheckoutRegistry::new();
    registry.attach_launch(1, &launch_id).unwrap();
    fs::create_dir(selected.join("nested")).unwrap();
    let chosen = registry
        .select(1, &launch_id, &selected.join("nested"))
        .unwrap();
    assert_eq!(chosen.root, selected);
    assert_ne!(chosen.root_key, launch_id.root_key);
    assert_eq!(registry.active_count(), 2);
    registry.disconnect(1);
    assert_eq!(registry.active_count(), 2);
    assert_eq!(registry.known_roots().len(), 2);
    assert!(registry.release(&chosen.root_key).unwrap());
    assert!(registry.release(&launch_id.root_key).unwrap());
    assert_eq!(registry.active_count(), 0);
}

#[test]
fn selection_does_not_follow_symlinks_or_admit_other_repositories() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let launch = base.join("launch");
    let other = base.join("other");
    fs::create_dir(&launch).unwrap();
    fs::create_dir(&other).unwrap();
    git(&["init", "-q"], &launch);
    git(&["init", "-q"], &other);
    let launch_id = identity(&launch);
    let mut registry = CheckoutRegistry::new();
    assert_eq!(
        registry
            .select(1, &launch_id, Path::new("other"))
            .unwrap_err(),
        SelectionError::NotAbsolute
    );
    assert_eq!(
        registry.select(1, &launch_id, &other).unwrap_err(),
        SelectionError::DifferentRepository
    );
    let alias = base.join("alias");
    symlink(&other, &alias).unwrap();
    assert_eq!(
        registry.select(1, &launch_id, &alias).unwrap_err(),
        SelectionError::NotCheckout
    );
    assert_eq!(registry.active_count(), 0);
    // A failed selection does not attach the rejected worktree or consume a slot.
    registry.attach_launch(1, &launch_id).unwrap();
    assert_eq!(
        registry.select(1, &launch_id, &other).unwrap_err(),
        SelectionError::DifferentRepository
    );
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.known_roots().len(), 1);
    registry.disconnect(1);
    assert_eq!(registry.active_count(), 1); // release is a separate decision
}

#[test]
fn inert_registration_and_capacity() {
    let temp = tempfile::tempdir().unwrap();
    let mut registry = CheckoutRegistry::new();
    let base = fs::canonicalize(temp.path()).unwrap();
    let mut identities = Vec::new();
    for index in 0..=MAX_ACTIVE_CHECKOUTS {
        let root = base.join(format!("root-{index}"));
        fs::create_dir(&root).unwrap();
        identities.push(identity(&root));
    }
    let options = CheckoutOptions(serde_json::json!({"provider":"first"}));
    registry.register(&identities[0], options.clone()).unwrap();
    registry.register(&identities[0], options.clone()).unwrap();
    assert_eq!(registry.active_count(), 0);
    assert_eq!(
        registry.registration(&identities[0].root_key),
        Some(&options)
    );
    for (index, identity) in identities.iter().take(MAX_ACTIVE_CHECKOUTS).enumerate() {
        registry.attach_launch(index as u64, identity).unwrap();
    }
    assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
    assert_eq!(
        registry
            .attach_launch(999, &identities[MAX_ACTIVE_CHECKOUTS])
            .unwrap_err(),
        SelectionError::CheckoutCapacity
    );
    assert!(SelectionError::CheckoutCapacity.retryable());
    assert_eq!(
        registry
            .register(
                &identities[0],
                CheckoutOptions(serde_json::json!({"provider":"other"}))
            )
            .unwrap_err(),
        SelectionError::RegistrationConflict
    );
    registry.disconnect(0);
    assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
    assert_eq!(
        registry
            .attach_launch(999, &identities[MAX_ACTIVE_CHECKOUTS])
            .unwrap_err(),
        SelectionError::CheckoutCapacity
    );
    let replacement = CheckoutOptions(serde_json::json!({"provider":"other"}));
    assert_eq!(
        registry
            .register(&identities[0], replacement.clone())
            .unwrap_err(),
        SelectionError::RegistrationConflict
    );
    registry
        .set_pending_work(&identities[0].root_key, true)
        .unwrap();
    assert!(!registry.release(&identities[0].root_key).unwrap());
    assert_eq!(
        registry
            .register(&identities[0], replacement.clone())
            .unwrap_err(),
        SelectionError::RegistrationConflict
    );
    registry
        .set_pending_work(&identities[0].root_key, false)
        .unwrap();
    assert!(registry.release(&identities[0].root_key).unwrap());
    registry
        .attach_launch(999, &identities[MAX_ACTIVE_CHECKOUTS])
        .unwrap();
    assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
    registry
        .register(
            &identities[0],
            CheckoutOptions(serde_json::json!({"provider":"other"})),
        )
        .unwrap();
}

#[test]
fn linked_worktree_witness_refuses_common_directory_drift() {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let launch = base.join("launch");
    let selected = base.join("selected");
    let foreign = base.join("foreign");
    fs::create_dir(&launch).unwrap();
    fs::create_dir(&foreign).unwrap();
    git(&["init", "-q"], &launch);
    git(&["init", "-q"], &foreign);
    git(
        &[
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.org",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
        &launch,
    );
    git(
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "selected",
            selected.to_str().unwrap(),
        ],
        &launch,
    );
    let launch_id = identity(&launch);
    let mut registry = CheckoutRegistry::new();
    let witness = registry.select(1, &launch_id, &selected).unwrap();
    witness.before_answer().unwrap();
    let git_dir = witness.git_dir().unwrap();
    fs::write(
        git_dir.join("commondir"),
        format!("{}\n", foreign.join(".git").display()),
    )
    .unwrap();
    assert_eq!(
        witness.before_answer().unwrap_err(),
        SelectionError::IdentityChanged
    );
    fs::write(git_dir.join("commondir"), "missing-common-dir\n").unwrap();
    let error = witness.before_answer().unwrap_err();
    assert_eq!(error, SelectionError::Unavailable);
    assert_eq!(error.reason(), "unavailable");
    assert!(error.retryable());
    registry.disconnect(1);
    assert!(registry.release(&witness.root_key).unwrap());
    registry.attach_launch(2, &launch_id).unwrap();
    let failed = registry.select(2, &launch_id, &selected).unwrap_err();
    assert_eq!(failed, SelectionError::Unavailable);
    // Failed admission neither attaches this checkout nor consumes another slot.
    assert_eq!(registry.active_count(), 1);
    assert_eq!(registry.known_roots().len(), 2);
}

#[test]
fn stalled_git_common_directory_lookup_is_bounded() {
    use std::{os::unix::fs::PermissionsExt, time::Instant};
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let root = base.join("checkout");
    fs::create_dir(&root).unwrap();
    git(&["init", "-q"], &root);
    let identity = identity(&root);
    let script = base.join("stalled-git");
    fs::write(&script, "#!/bin/sh\nexec /bin/sleep 5\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let start = Instant::now();
    let error = identity
        .git_common_dir_with_executable(&script)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unavailable: Git common-directory lookup timed out"),
        "{error}"
    );
    // The typed timeout above proves the lookup bound fired; this detects a hang.
    assert!(start.elapsed() < trellis::testing::HANG_BACKSTOP);
}

#[test]
fn inherited_stdout_does_not_extend_git_deadline() {
    use std::{os::unix::fs::PermissionsExt, time::Instant};
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let root = base.join("checkout");
    fs::create_dir(&root).unwrap();
    git(&["init", "-q"], &root);
    let identity = identity(&root);
    let script = base.join("wrapper-git");
    // The wrapper is killed while its ordinary child still has stdout open.
    fs::write(&script, "#!/bin/sh\n/bin/sleep 2 &\nwait\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let start = Instant::now();
    let error = identity
        .git_common_dir_with_executable(&script)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unavailable: Git common-directory lookup timed out"),
        "{error}"
    );
    // The typed timeout above proves the lookup bound fired; this detects a hang.
    assert!(start.elapsed() < trellis::testing::HANG_BACKSTOP);
}
