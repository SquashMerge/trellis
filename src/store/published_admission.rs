//! Successful published-head admission, shared by this Store's response clones.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdmissionKey {
    pin: IndexPin,
    device: u64,
    inode: u64,
    change_counter: u32,
}

/// One committed state of the live index file whose bounded control validation passed in
/// one scope (#1245): the file the connection opened and SQLite's file change counter.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ControlState {
    device: u64,
    inode: u64,
    change_counter: u32,
    scope: PairedManifestScope,
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
    /// Bounded control validations the #1245 memo let run on a known file state.
    #[cfg(test)]
    pub(super) control_validations: std::sync::atomic::AtomicUsize,
    /// Status semantic projections this Store and its clones have run (#672).
    #[cfg(feature = "test-support")]
    projections: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct AdmissionState {
    epoch: u64,
    entry: Option<(AdmissionKey, IndexStatus)>,
    /// The same head's status with its semantic projection (#672): the control status it
    /// was projected from and the result.
    semantics: Option<(AdmissionKey, IndexStatus, IndexStatus)>,
    /// Scopes whose bounded control validation passed, all for one committed file state.
    control: Vec<ControlState>,
}

impl PublishedAdmission {
    pub(super) fn invalidate(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.epoch = state.epoch.wrapping_add(1);
        state.entry = None;
        state.semantics = None;
        state.control.clear();
    }
}

/// SQLite's file change counter (header bytes 24..28) of `db`'s main database, read
/// through SQLite's own file handle for it. No second descriptor is opened, so none is
/// closed on the live file (#67).
pub(super) fn sqlite_file_change_counter(db: &Connection) -> Result<u32> {
    use rusqlite::ffi;
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    // SAFETY: `db` is open and used only on this thread (SQLITE_OPEN_NO_MUTEX). The file
    // control writes the main database's `sqlite3_file`, which `db` owns while it is open.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            db.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            (&raw mut file).cast(),
        )
    };
    ensure!(
        rc == ffi::SQLITE_OK && !file.is_null(),
        "SQLite file pointer unavailable"
    );
    // SAFETY: as above; a closed file has no methods, and then nothing is read.
    let read = unsafe { (*file).pMethods.as_ref() }
        .and_then(|methods| methods.xRead)
        .context("SQLite database file is not open")?;
    let mut counter = [0u8; 4];
    // SAFETY: as above; xRead fills at most the 4 bytes of `counter`.
    let rc = unsafe { read(file, counter.as_mut_ptr().cast(), 4, 24) };
    ensure!(rc == ffi::SQLITE_OK, "SQLite change counter read failed");
    Ok(u32::from_be_bytes(counter))
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

    /// Bounded control validation of the live index, skipped when this Store already
    /// validated the same committed file state in the same scope (#1245, owner ruling
    /// 2026-11-20 extending #261). `file` is the (device, inode) the connection opened.
    /// The state is keyed on SQLite's file change counter, read while `db`'s read
    /// transaction holds SHARED: under the DELETE journal no writer can change the file
    /// during the snapshot, and every COMMIT, by any connection, advances the counter.
    /// Any doubt validates: no known file, a writable or autocommit connection, or an
    /// unreadable counter. Only a pass is remembered; a reopened Store starts empty.
    pub(super) fn validate_bounded_control_once(
        &self,
        db: &Connection,
        file: Option<(u64, u64)>,
        scope: PairedManifestScope,
    ) -> Result<()> {
        let state = match file {
            Some((device, inode))
                if !db.is_autocommit() && db.is_readonly(rusqlite::MAIN_DB).unwrap_or(false) =>
            {
                sqlite_file_change_counter(db)
                    .ok()
                    .map(|change_counter| ControlState {
                        device,
                        inode,
                        change_counter,
                        scope,
                    })
            }
            _ => None,
        };
        let Some(state) = state else {
            return validate_bounded_control(db, &self.identity.record_id, scope);
        };
        let epoch = {
            let admission = self
                .published_admission
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if admission.control.contains(&state) {
                return Ok(());
            }
            admission.epoch
        };
        #[cfg(test)]
        self.published_admission
            .control_validations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        validate_bounded_control(db, &self.identity.record_id, scope)?;
        let mut admission = self
            .published_admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if admission.epoch == epoch {
            // Passes of another file state are stale: keep only this state's scopes.
            admission.control.retain(|seen| {
                (seen.device, seen.inode, seen.change_counter)
                    == (state.device, state.inode, state.change_counter)
            });
            if !admission.control.contains(&state) {
                admission.control.push(state);
            }
        }
        Ok(())
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

    /// `status` with its per-language semantic projection (#672). The projection reads only
    /// the snapshot, so it is remembered for the same admission key as the control status:
    /// the published pin and the index file's commit counter. Any commit, including an N or
    /// S publication, changes the key; a read of another pin or snapshot never sees it.
    pub(super) fn status_with_semantics(
        &self,
        db: &Connection,
        mut status: IndexStatus,
    ) -> Result<IndexStatus> {
        let Some(key) = self.admission_key(db)? else {
            self.project_status_semantics(db, &mut status)?;
            return Ok(status);
        };
        let epoch = {
            let state = self
                .published_admission
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some((cached, control, projected)) = &state.semantics
                && *cached == key
                && *control == status
            {
                return Ok(projected.clone());
            }
            state.epoch
        };
        let control = status.clone();
        self.project_status_semantics(db, &mut status)?;
        let mut state = self
            .published_admission
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if state.epoch == epoch {
            state.semantics = Some((key, control, status.clone()));
        }
        Ok(status)
    }

    fn project_status_semantics(&self, db: &Connection, status: &mut IndexStatus) -> Result<()> {
        #[cfg(feature = "test-support")]
        self.published_admission
            .projections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        semantic_display::status_semantics(db, status)
    }

    /// How many status semantic projections this Store and its clones have run (#672).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn status_semantic_projections_for_tests(&self) -> usize {
        self.published_admission
            .projections
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}
