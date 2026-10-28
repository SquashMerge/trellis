//! Successful published-head admission, shared by this Store's response clones.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdmissionKey {
    pin: IndexPin,
    device: u64,
    inode: u64,
    change_counter: u32,
}

#[derive(Default)]
pub(super) struct PublishedAdmission {
    state: Mutex<AdmissionState>,
    // Only one reader validates a missed head. Invalidation never takes this
    // lock, so corruption/recovery cannot wait for an in-flight validator.
    validator: Mutex<()>,
    #[cfg(test)]
    pub(super) before_lookup: TestOneShotHook,
    #[cfg(test)]
    pub(super) before_remember: TestOneShotHook,
}

#[derive(Default)]
struct AdmissionState {
    epoch: u64,
    entry: Option<(AdmissionKey, IndexStatus)>,
}

impl PublishedAdmission {
    pub(super) fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        state.entry = None;
    }
}

impl Store {
    /// SQLite's persisted change counter is comparable across fresh connections,
    /// unlike PRAGMA data_version. DELETE-journal readers hold SHARED throughout
    /// this snapshot, so no writer can change the header between key and rows.
    /// Use the retained descriptor: closing another FD would drop SQLite locks.
    fn admission_key(&self, db: &Connection) -> Result<Option<AdmissionKey>> {
        use std::os::unix::fs::{FileExt, MetadataExt};
        // Recovery and publication also decode status on writable connections;
        // their uncommitted rows must never enter the published-head cache.
        if db.is_autocommit() || !db.is_readonly(rusqlite::MAIN_DB)? {
            return Ok(None);
        }
        let pin = ReadRevision::current(db)?.pin;
        let file = retained_sqlite_file(&self.roots.index_db(&self.identity), false, false, false)?;
        let metadata = file.metadata()?;
        let mut counter = [0; 4];
        file.read_exact_at(&mut counter, 24)?;
        Ok(Some(AdmissionKey {
            pin,
            device: metadata.dev(),
            inode: metadata.ino(),
            change_counter: u32::from_be_bytes(counter),
        }))
    }

    fn check_admission_readiness(&self, pre_startup: bool) -> Result<()> {
        if pre_startup {
            // A daemon may read the valid prior head during startup, but never
            // after a selected read has proved it corrupt.
            if self.disposition() == RecoveryDisposition::Rebuild {
                anyhow::bail!(
                    "incompatible_index: reconciliation required after invalid current index"
                );
            }
            Ok(())
        } else {
            self.ensure_public_read_ready()
        }
    }

    pub(super) fn cached_read_status_for(
        &self,
        db: &Connection,
        pre_startup: bool,
    ) -> Result<IndexStatus> {
        self.verify_metadata_root(db)?;
        self.check_admission_readiness(pre_startup)?;
        let Some(key) = self.admission_key(db)? else {
            return self.read_status_uncached(db, pre_startup);
        };
        #[cfg(test)]
        self.published_admission.before_lookup.run();
        let _validator = self
            .published_admission
            .validator
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.check_admission_readiness(pre_startup)?;
        let epoch = {
            let state = self
                .published_admission
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((cached, status)) = &state.entry
                && *cached == key
            {
                return Ok(status.clone());
            }
            state.epoch
        };
        let status = self.read_status_uncached(db, pre_startup)?;
        #[cfg(test)]
        self.published_admission.before_remember.run();
        self.check_admission_readiness(pre_startup)?;
        let mut state = self
            .published_admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.epoch == epoch {
            state.entry = Some((key, status.clone()));
        }
        Ok(status)
    }
}
