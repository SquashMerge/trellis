//! GC of producer state with its index, and the orphan rule (D2).
#![cfg(feature = "test-support")]
use crate::common;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, sync::Arc};
use tempfile::TempDir;
use trellis::{
    producer::state::StateDir,
    store::topology::{TopologyRoots, WorkspaceIdentity},
};

const NOW: i64 = 1_800_000_000;

struct World {
    temp: TempDir,
    roots: TopologyRoots,
}

impl World {
    fn new() -> Self {
        let (temp, roots) = common::fixture();
        Self { temp, roots }
    }

    fn workspace(&self, name: &str) -> (std::path::PathBuf, WorkspaceIdentity) {
        let path = self.temp.path().join(name);
        fs::create_dir(&path).unwrap();
        let identity = WorkspaceIdentity::discover(Some(&path), &path).unwrap();
        (path, identity)
    }

    /// An index of an earlier schema: always GC-eligible, whatever its age.
    fn old_index(&self, name: &str) -> WorkspaceIdentity {
        let (_, identity) = self.workspace(name);
        drop(self.roots.index_use(&identity).unwrap());
        let path = self.roots.index_db(&identity);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE anything(x)").unwrap();
        db.pragma_update(None, "user_version", 7_i64).unwrap();
        drop(db);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        identity
    }

    /// A current index that opened recently: never GC-eligible.
    fn recent_index(&self, name: &str) -> WorkspaceIdentity {
        let (path, identity) = self.workspace(name);
        drop(common::open_store(self.temp.path(), &path).unwrap());
        drop(
            self.roots
                .writer(Arc::new(identity.verified_clone_for_tests().unwrap()))
                .unwrap(),
        );
        identity
    }

    /// The checkout that runs GC; its own producer state must survive.
    fn current(&self) -> (WorkspaceIdentity, trellis::store::topology::WriterGuard) {
        let (path, identity) = self.workspace("current");
        drop(common::open_store(self.temp.path(), &path).unwrap());
        let writer = self
            .roots
            .writer(Arc::new(identity.verified_clone_for_tests().unwrap()))
            .unwrap();
        (identity, writer)
    }

    fn state(&self, identity: &WorkspaceIdentity) -> StateDir {
        let state = StateDir::open(&self.roots, identity).unwrap();
        let run = state.create_run_dir(&format!("{:032x}", 1)).unwrap();
        fs::create_dir(run.join("out")).unwrap();
        fs::write(run.join("out/index.scip"), b"artifact").unwrap();
        state
    }

    fn orphan_state(&self, key: &str) -> std::path::PathBuf {
        let producers = self.roots.cache.join("producers");
        if !producers.exists() {
            fs::create_dir(&producers).unwrap();
            fs::set_permissions(&producers, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = producers.join(key);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("pickup.json"), b"{}").unwrap();
        path
    }
}

fn restore(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn gc_removes_producer_state_with_its_index() {
    let world = World::new();
    let (current, writer) = world.current();
    let old = world.old_index("old");
    let old_state = world.state(&old);
    let current_state = world.state(&current);
    assert_eq!(
        world.roots.automatic_gc_at(&current, &writer, NOW).unwrap(),
        1
    );
    assert!(!world.roots.index_dir(&old).exists());
    assert!(!world.roots.index_use_lock(&old).exists());
    assert!(!old_state.path().exists());
    assert!(
        current_state.path().exists(),
        "the running checkout keeps its producer state"
    );
}

#[test]
fn gc_skips_candidate_with_unsafe_producer_state() {
    let world = World::new();
    let (current, writer) = world.current();
    let old = world.old_index("old");
    let state = world.state(&old);
    fs::set_permissions(state.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        world.roots.automatic_gc_at(&current, &writer, NOW).unwrap(),
        0
    );
    assert!(world.roots.index_db(&old).exists(), "the index stays");
    assert!(
        state.path().join("runs").exists(),
        "unsafe state is not touched"
    );
    assert_eq!(
        fs::metadata(state.path()).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn producer_state_removal_failure_is_reported_then_collected_as_orphan() {
    let world = World::new();
    let (current, writer) = world.current();
    let old = world.old_index("old");
    let state = world.state(&old);
    // An entry that cannot be unlinked makes remove_dir_all fail.
    let runs = state.path().join("runs");
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o500)).unwrap();
    let error = world
        .roots
        .automatic_gc_at(&current, &writer, NOW)
        .unwrap_err();
    assert!(format!("{error:#}").contains("remove"), "{error:#}");
    // Past the unlink boundary: the index is gone, the lock and state remain.
    assert!(!world.roots.index_dir(&old).exists());
    assert!(world.roots.index_use_lock(&old).exists());
    assert!(state.path().exists());

    restore(&runs);
    assert_eq!(
        world
            .roots
            .automatic_gc_at(&current, &writer, NOW + 86_400)
            .unwrap(),
        0,
        "orphans do not count toward the removed indexes"
    );
    assert!(!state.path().exists());
    assert!(!world.roots.index_use_lock(&old).exists());
}

#[test]
fn orphan_producer_state_without_index_is_collected() {
    let world = World::new();
    let (current, writer) = world.current();
    let key = "a".repeat(64);
    let orphan = world.orphan_state(&key);
    // Not a root key: left alone.
    let stray = world.roots.cache.join("producers").join("not-a-key");
    fs::create_dir(&stray).unwrap();
    // Not private: refused, never repaired.
    let unsafe_key = "b".repeat(64);
    let unsafe_orphan = world.orphan_state(&unsafe_key);
    fs::set_permissions(&unsafe_orphan, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        world.roots.automatic_gc_at(&current, &writer, NOW).unwrap(),
        0
    );
    assert!(!orphan.exists());
    assert!(stray.exists());
    assert!(unsafe_orphan.exists());
    let indexes = world.roots.cache.join("indexes");
    assert!(!indexes.join(format!("{key}.lock")).exists());
    assert!(!indexes.join(format!("{unsafe_key}.lock")).exists());
}

#[test]
fn orphan_rule_skips_current_and_indexed_keys() {
    let world = World::new();
    let (current, writer) = world.current();
    let recent = world.recent_index("recent");
    let current_state = world.state(&current);
    let recent_state = world.state(&recent);
    // The recent index must stay recent, so use the real clock.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(
        world.roots.automatic_gc_at(&current, &writer, now).unwrap(),
        0
    );
    assert!(current_state.path().exists());
    assert!(
        recent_state.path().exists(),
        "an existing index owns its producer state"
    );
    assert!(world.roots.index_dir(&recent).exists());
}
