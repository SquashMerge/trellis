use std::{fs, path::Path};
use tempfile::TempDir;
use trellis::store::{
    Store,
    topology::{TopologyRoots, UseGuard},
};

#[allow(dead_code)]
pub fn fixture() -> (TempDir, TopologyRoots) {
    let dir = tempfile::tempdir().unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(dir.path().join("cache"), dir.path().join("data"));
    (dir, roots)
}
#[allow(dead_code)]
pub fn private(path: &Path) {
    fs::create_dir(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

/// Every direct Store fixture exercises the fixed placement and real inode locks.
#[allow(dead_code)]
pub fn open_store(state: &Path, workspace: &Path) -> anyhow::Result<Store> {
    let durable_existed = state.join("data/workspaces").try_exists().unwrap_or(false);
    let store = Store::open_for_tests(state, workspace)?;
    assert_topology_fixture(state, workspace, &store, durable_existed);
    Ok(store)
}
#[allow(dead_code)]
pub fn assert_topology_fixture(
    state: &Path,
    workspace: &Path,
    store: &Store,
    durable_existed: bool,
) {
    use std::os::unix::fs::PermissionsExt;
    let cache = state.join("cache");
    let indexes = cache.join("indexes");
    let root = workspace.canonicalize().unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(workspace), workspace).unwrap();
    assert_eq!(identity.root, root);
    store.verify_root().unwrap();
    let baseline = store.index_baseline().unwrap();
    assert_eq!(baseline.index_revision, 0);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let entries: Vec<_> = fs::read_dir(&indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let index = entries.iter().find(|path| path.is_dir()).unwrap();
    assert_eq!(index.parent().unwrap(), indexes);
    for path in [&cache, &indexes, index] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",
            path.display()
        );
    }
    for path in [
        index.join("index.db"),
        indexes.join(format!(
            "{}.lock",
            index.file_name().unwrap().to_string_lossy()
        )),
    ] {
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "{}",
            path.display()
        );
    }
    let lock = indexes.join(format!(
        "{}.lock",
        index.file_name().unwrap().to_string_lossy()
    ));
    let held = UseGuard::acquire_existing(&lock, false, false).unwrap();
    assert!(
        UseGuard::acquire_existing(&lock, true, true).is_err(),
        "shared use must block exclusive deletion"
    );
    drop(held);
    assert_ne!(cache, state.join("data"));
    if !durable_existed {
        let data = state.join("data/workspaces");
        assert!(
            !data.exists()
                || !fs::read_dir(data).unwrap().any(|entry| entry
                    .unwrap()
                    .path()
                    .join("workspace.db")
                    .exists()),
            "Store open must not create durable records"
        );
    }
}
