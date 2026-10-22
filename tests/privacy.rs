use std::fs;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn state_and_databases_are_private_and_symlink_safe() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let state = temp.path().join("private-state");
    let store = crate::common::open_store(&state, &source).unwrap();
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let indexes = state.join("cache/indexes");
    let index = fs::read_dir(&indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    assert_eq!(
        fs::metadata(index.join("index.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(index.with_extension("lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        !state.join("data/workspaces").exists(),
        "reads do not initialize durable data"
    );
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let public = temp.path().join("public");
    fs::create_dir(&public).unwrap();
    fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
    let public_store = crate::common::open_store(&public, &source).unwrap();
    assert_eq!(
        fs::metadata(public.join("cache"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    drop(public_store);
    let alias = temp.path().join("alias");
    symlink(&state, &alias).unwrap();
    // The existing parent alias is not itself a Trellis-managed component.
    assert!(crate::common::open_store(&alias, &source).is_ok());
    fs::remove_file(index.join("index.db")).unwrap();
    let other = temp.path().join("not-a-cache");
    fs::write(&other, b"sensitive").unwrap();
    symlink(&other, index.join("index.db")).unwrap();
    assert!(store.status().is_err());
    assert_eq!(fs::read(&other).unwrap(), b"sensitive");
}
