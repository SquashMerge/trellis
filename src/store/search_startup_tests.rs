//! Startup observations must not activate, repair, or refresh a checkout.
use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

struct Fixture {
    _root: tempfile::TempDir,
    store: Store,
    pin: IndexPin,
}

impl Fixture {
    fn published(state: &Path, age: u64) -> Self {
        let root = tempfile::tempdir().unwrap();
        git2::Repository::init(root.path()).unwrap();
        fs::write(
            root.path().join("orchard.rs"),
            "fn orchard() { apple(); }\n",
        )
        .unwrap();
        fs::write(root.path().join("steady.rs"), "fn steady() {}\n").unwrap();
        let store = Store::open_for_tests(state, root.path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                &store.writer().unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        store
            .cache_write()
            .unwrap()
            .execute(
                "UPDATE index_metadata SET last_opened_at=?1 WHERE singleton=1",
                [i64::try_from(age).unwrap()],
            )
            .unwrap();
        Self {
            _root: root,
            store,
            pin,
        }
    }

    fn checkout(&self, age: u64) -> RecentSearchCheckout {
        RecentSearchCheckout {
            root_key: self.store.identity.root_key.clone(),
            root: self.store.identity.root.clone(),
            last_opened_at: age,
        }
    }
}

fn roots(state: &Path) -> topology::TopologyRoots {
    topology::TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"))
}

// Do not open extra descriptors on SQLite files: closing one can release this
// process's live SQLite locks. Metadata also works while a sidecar writer lives.
// Access time is deliberately excluded because observing files may update it.
#[derive(Debug, PartialEq, Eq)]
struct FileState {
    device: u64,
    inode: u64,
    mode: u32,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn tree(path: &Path) -> BTreeMap<PathBuf, FileState> {
    fn visit(path: &Path, out: &mut BTreeMap<PathBuf, FileState>) {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("{}: {error}", path.display()),
        };
        out.insert(
            path.to_owned(),
            FileState {
                device: metadata.dev(),
                inode: metadata.ino(),
                mode: metadata.mode(),
                len: metadata.len(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed: (metadata.ctime(), metadata.ctime_nsec()),
            },
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), out);
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(path, &mut out);
    out
}

#[test]
fn existing_head_matches_shared_mcp_read_without_refreshing_or_writing() {
    let state = tempfile::tempdir().unwrap();
    let fixture = Fixture::published(state.path(), 700);
    let checkout = fixture.checkout(700);
    let expected = {
        let response = fixture.store.evidence_response().unwrap();
        let head = response
            .mcp_search_head(Some(fixture.pin), &mut || Ok(()))
            .unwrap();
        response.finish(head).unwrap()
    };
    assert_eq!(expected.1.len(), 2);
    let before_state = tree(state.path());
    let before_root = tree(&checkout.root);
    let discovered =
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 3).unwrap();
    assert_eq!(discovered, vec![checkout.clone()]);
    let actual =
        Store::read_existing_search_head(&roots(state.path()), &discovered[0], &mut || Ok(()))
            .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 3).unwrap(),
        discovered,
    );
    assert_eq!(tree(state.path()), before_state);
    assert_eq!(tree(&checkout.root), before_root);
    assert!(!fixture.store.recovery_pending_for_tests());
}

#[test]
fn discovery_uses_metadata_age_order_and_caller_bounds() {
    let state = tempfile::tempdir().unwrap();
    let ages = [0, 899, 900, 950, 950, 1_001];
    let fixtures: Vec<_> = ages
        .into_iter()
        .map(|age| Fixture::published(state.path(), age))
        .collect();
    let mut expected: Vec<_> = fixtures
        .iter()
        .zip(ages)
        .filter(|(_, age)| (900..=1_000).contains(age))
        .map(|(fixture, age)| fixture.checkout(age))
        .collect();
    expected.sort_by(|a, b| {
        b.last_opened_at
            .cmp(&a.last_opened_at)
            .then_with(|| a.root_key.cmp(&b.root_key))
    });
    let before = tree(state.path());
    assert_eq!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::from_secs(100), 9)
            .unwrap(),
        expected,
    );
    assert_eq!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::from_secs(100), 1)
            .unwrap(),
        expected[..1],
    );
    assert!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 0,)
            .unwrap()
            .is_empty()
    );
    let unbounded =
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 9).unwrap();
    assert_eq!(unbounded.len(), 4);
    assert_eq!(unbounded.last().unwrap().last_opened_at, 899);
    assert_eq!(tree(state.path()), before);
}

#[test]
fn missing_catalog_index_and_git_marker_are_never_created() {
    let state = tempfile::tempdir().unwrap();
    let empty = roots(&state.path().join("absent"));
    let before_empty = tree(state.path());
    assert!(
        Store::recent_search_checkouts(&empty, 1_000, Duration::MAX, 3)
            .unwrap()
            .is_empty()
    );
    assert_eq!(tree(state.path()), before_empty);

    let fixture = Fixture::published(state.path(), 700);
    let checkout = fixture.checkout(700);
    let index = fixture.store.roots.index_db(&fixture.store.identity);
    let marker = checkout.root.join(".git/trellis/workspace-id");
    let parked_index = index.with_file_name("published-index.saved");
    drop(fixture.store);
    // Remove a legitimate publication, rather than inventing a database layout.
    fs::rename(&index, &parked_index).unwrap();
    let before_state = tree(state.path());
    let before_root = tree(&checkout.root);
    assert!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        Store::read_existing_search_head(&roots(state.path()), &checkout, &mut || Ok(())).is_err()
    );
    assert_eq!(tree(state.path()), before_state);
    assert_eq!(tree(&checkout.root), before_root);
    assert!(!index.exists());

    fs::rename(&parked_index, &index).unwrap();
    fs::remove_file(&marker).unwrap();
    let before_state = tree(state.path());
    let before_root = tree(&checkout.root);
    assert!(
        Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 3)
            .unwrap()
            .is_empty()
    );
    assert!(
        Store::read_existing_search_head(&roots(state.path()), &checkout, &mut || Ok(())).is_err()
    );
    assert_eq!(tree(state.path()), before_state);
    assert_eq!(tree(&checkout.root), before_root);
    assert!(index.exists());
    assert!(!marker.exists());
}

#[test]
fn sqlite_journal_and_wal_sidecars_refuse_without_recovery_or_new_files() {
    for wal in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let fixture = Fixture::published(state.path(), 700);
        let checkout = fixture.checkout(700);
        let index = fixture.store.roots.index_db(&fixture.store.identity);
        // Produce real SQLite sidecars on a normal published index. Keep the
        // protected writer alive so its journal/WAL cannot disappear mid-probe.
        let writer = fixture.store.cache_write().unwrap();
        if wal {
            let mode: String = writer
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .unwrap();
            assert_eq!(mode, "wal");
            writer
                .execute(
                    "UPDATE index_metadata SET last_opened_at=701 WHERE singleton=1",
                    [],
                )
                .unwrap();
            assert!(index.with_file_name("index.db-wal").exists());
            assert!(index.with_file_name("index.db-shm").exists());
        } else {
            writer.execute_batch(
                "BEGIN IMMEDIATE; UPDATE index_metadata SET last_opened_at=701 WHERE singleton=1",
            ).unwrap();
            assert!(index.with_file_name("index.db-journal").exists());
        }
        let before_state = tree(state.path());
        let before_root = tree(&checkout.root);
        assert!(
            Store::recent_search_checkouts(&roots(state.path()), 1_000, Duration::MAX, 3,)
                .unwrap()
                .is_empty()
        );
        let error =
            Store::read_existing_search_head(&roots(state.path()), &checkout, &mut || Ok(()))
                .unwrap_err();
        assert!(error.is::<topology::StorageBusy>(), "{error:#}");
        assert_eq!(tree(state.path()), before_state);
        assert_eq!(tree(&checkout.root), before_root);
        assert!(!fixture.store.recovery_pending_for_tests());
        if !wal {
            writer.execute_batch("ROLLBACK").unwrap();
        }
    }
}

#[test]
fn checkout_replaced_during_head_read_is_refused_without_writes() {
    let state = tempfile::tempdir().unwrap();
    let fixture = Fixture::published(state.path(), 700);
    let checkout = fixture.checkout(700);
    let moved = checkout.root.with_extension("startup-moved");
    let mut checks = 0;
    let mut after_replacement = None;
    let error = Store::read_existing_search_head(&roots(state.path()), &checkout, &mut || {
        checks += 1;
        if checks == 2 {
            fs::rename(&checkout.root, &moved).unwrap();
            fs::create_dir(&checkout.root).unwrap();
            after_replacement = Some((tree(state.path()), tree(&checkout.root), tree(&moved)));
        }
        Ok(())
    })
    .unwrap_err();
    let (before_state, before_root, before_moved) = after_replacement.unwrap();
    assert!(error.to_string().contains("root_changed"), "{error:#}");
    assert_eq!(tree(state.path()), before_state);
    assert_eq!(tree(&checkout.root), before_root);
    assert_eq!(tree(&moved), before_moved);
    fs::remove_dir(&checkout.root).unwrap();
    fs::rename(&moved, &checkout.root).unwrap();
}
