//! Observational startup discovery and head reads; never activate a checkout.
use super::*;
use crate::search_rank::SearchCandidate;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentSearchCheckout {
    pub root_key: String,
    pub root: std::path::PathBuf,
    pub last_opened_at: u64,
}

impl Store {
    /// Discover recent existing indexes without creating a catalog or refreshing
    /// their open age. Unavailable candidates are not startup work.
    pub fn recent_search_checkouts(
        roots: &topology::TopologyRoots,
        now: u64,
        max_age: Duration,
        limit: usize,
    ) -> Result<Vec<RecentSearchCheckout>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let entries = match std::fs::read_dir(roots.cache.join("indexes")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut recent = Vec::new();
        for entry in entries.flatten() {
            let Some(key) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(spelling) = Self::browser_index_root_existing(roots, &key) else {
                continue;
            };
            let mut record = RecentSearchCheckout {
                root_key: key,
                root: spelling.into(),
                last_opened_at: 0,
            };
            let observed = with_existing_search_store(roots, &record, |store, db| {
                let age: i64 = storage_result(db.query_row(
                    "SELECT last_opened_at FROM index_metadata WHERE singleton=1",
                    [],
                    |row| row.get(0),
                ))?;
                store.identity.verify_readonly()?;
                db._use_guard.verify()?;
                Ok(age)
            });
            let Ok(age) = observed.and_then(|age| Ok(u64::try_from(age)?)) else {
                continue;
            };
            if age == 0 || age > now || now - age > max_age.as_secs() {
                continue;
            }
            record.last_opened_at = age;
            recent.push(record);
        }
        recent.sort_by(|a, b| {
            b.last_opened_at
                .cmp(&a.last_opened_at)
                .then_with(|| a.root_key.cmp(&b.root_key))
        });
        recent.truncate(limit);
        Ok(recent)
    }

    pub(crate) fn read_existing_search_pin(
        roots: &topology::TopologyRoots,
        checkout: &RecentSearchCheckout,
    ) -> Result<IndexPin> {
        with_existing_search_store(roots, checkout, |store, db| {
            storage_result(db.execute_batch("BEGIN DEFERRED"))?;
            begin_read_snapshot(
                &db,
                &roots.index_db(&store.identity),
                Duration::from_secs(5),
            )?;
            let selected = store.read_revision_for(&db, None, true)?;
            store.identity.verify_readonly()?;
            db._use_guard.verify()?;
            Ok(selected.pin)
        })
    }

    /// Read candidate identities only. Startup work owns no registry admission,
    /// writer session, watcher, or long-lived SQLite connection.
    pub fn read_existing_search_head(
        roots: &topology::TopologyRoots,
        checkout: &RecentSearchCheckout,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(IndexPin, Arc<[SearchCandidate]>)> {
        Self::read_existing_search_head_admitted(roots, checkout, check, &mut |_| Ok(()))
    }

    pub(crate) fn read_existing_search_head_admitted(
        roots: &topology::TopologyRoots,
        checkout: &RecentSearchCheckout,
        check: &mut impl FnMut() -> Result<()>,
        admit: &mut dyn FnMut(usize) -> Result<()>,
    ) -> Result<(IndexPin, Arc<[SearchCandidate]>)> {
        check()?;
        with_existing_search_store(roots, checkout, |mut store, db| {
            storage_result(db.execute_batch("BEGIN DEFERRED"))?;
            begin_read_snapshot(
                &db,
                &roots.index_db(&store.identity),
                Duration::from_secs(5),
            )?;
            let selected = store.read_revision_for(&db, None, true)?;
            store.identity.verify_readonly()?;
            store.admitted = Some((selected.pin, selected.headers));
            let response = EvidenceResponse {
                optional_recovery: OptionalRecovery::new(&store),
                store,
                db,
                fence: ReadFence(selected.pin),
            };
            let value = response.mcp_search_head_admitted(Some(selected.pin), check, admit)?;
            check()?;
            // The ordinary response fence opens the Git marker read/write.
            // Preserve its selected-snapshot check using readonly identity opens.
            response.store.identity.verify_readonly()?;
            let status = response.store.read_status_for(&response.db, true)?;
            ensure!(
                status.revision == selected.pin,
                "index_not_ready: selected snapshot changed"
            );
            response.db._use_guard.verify()?;
            response.store.identity.verify_readonly()?;
            Ok(value)
        })
    }
}

fn with_existing_search_store<T>(
    roots: &topology::TopologyRoots,
    checkout: &RecentSearchCheckout,
    read: impl FnOnce(Store, IndexConnection) -> Result<T>,
) -> Result<T> {
    let identity =
        topology::WorkspaceIdentity::discover_unattached(Some(&checkout.root), &checkout.root)?
            .attach_existing_marker_readonly()?;
    ensure!(
        identity.root_key == checkout.root_key && identity.root == checkout.root,
        "root_changed: startup checkout identity changed"
    );
    roots.reject_root_overlap(&identity)?;
    let store = Store::unopened(roots.clone(), identity)?;
    let index = roots.index_db(&store.identity);
    let observed = (|| {
        let guard = roots.index_use_existing_readonly(&store.identity)?;
        // A readonly SQLite open must not roll back a hot journal, create WAL
        // sidecars, or ignore the live publication represented by those files.
        for suffix in ["-wal", "-shm", "-journal"] {
            match std::fs::symlink_metadata(index.with_file_name(format!("index.db{suffix}"))) {
                Ok(_) => return Err(topology::StorageBusy.into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        let db = open_index(&index, false)?;
        store.identity.verify_readonly()?;
        guard.verify()?;
        read(
            store,
            IndexConnection {
                db,
                _use_guard: guard,
            },
        )
    })();
    // Retained header descriptors may close only after an idle exclusive proof;
    // the shared process-wide witness keeps concurrent SQLite locks intact.
    let lock = roots
        .cache
        .join("indexes")
        .join(format!("{}.lock", checkout.root_key));
    if let Ok(_exclusive) = topology::UseGuard::acquire_existing_readonly_exclusive(&lock) {
        drop(SqliteWitnessRetirement(vec![index]));
    }
    observed
}

#[cfg(test)]
#[path = "search_startup_tests.rs"]
mod startup_tests;
