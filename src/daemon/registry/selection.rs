//! Checkout selection: preparing and committing a selection, the capacity witness and the
//! selected checkout's final checks.
use super::{
    CheckoutRegistry, CommonDirs, PreparedSelection, SelectedCheckout, SelectionError,
    SelectionProbe, common_identity_unattached,
};
use crate::store::topology::WorkspaceIdentity;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

impl SelectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::StorageBusy => "storage_busy",
            Self::CheckoutCapacity => "checkout_capacity",
            Self::RegistrationConflict => "registration_conflict",
            _ => "workspace_selection_failed",
        }
    }
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotAbsolute => "not_absolute",
            Self::NotCheckout => "not_checkout",
            Self::IdentityChanged => "identity_changed",
            Self::Unavailable => "unavailable",
            Self::StorageBusy => "storage_busy",
            Self::CheckoutCapacity => "checkout_capacity",
            Self::RegistrationConflict => "registration_conflict",
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::StorageBusy | Self::CheckoutCapacity
        )
    }
}

impl PreparedSelection {
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.witness.identity
    }
}

impl SelectionProbe {
    pub fn prepare(
        &self,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<PreparedSelection, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        let root = checkout_root(selected)?;
        // Any verified checkout may be selected (#839); its Git common directory
        // is captured before marker attachment so later checks can see it change.
        let unattached = WorkspaceIdentity::discover_unattached(Some(&root), &root)
            .map_err(|_| SelectionError::Unavailable)?;
        let launch_common = common_identity(launch)?;
        let selected_common = common_identity_unattached(&unattached)?;
        let identity = unattached
            .attach_marker()
            .map_err(|_| SelectionError::Unavailable)?;
        // The worktree and its common directory can change while Git runs.
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        if common_identity(launch)? != launch_common
            || common_identity(&identity)? != selected_common
        {
            return Err(SelectionError::IdentityChanged);
        }
        // Complete all fallible membership checks before reserving a session slot.
        // A failed selection must not attach or consume capacity.
        let witness = SelectedCheckout {
            launch: Arc::new(
                launch
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            identity: Arc::new(
                identity
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            launch_common,
            selected_common,
        };
        witness.before_answer()?;
        let expected = self.entries.get(&witness.root_key).cloned();
        Ok(PreparedSelection { witness, expected })
    }
}

impl CheckoutRegistry {
    /// The caller retains its launch identity for the lifetime of its connection.
    /// This operation does not open a Store or elect a checkout leader.
    pub fn attach_launch(
        &mut self,
        session: u64,
        launch: &WorkspaceIdentity,
    ) -> Result<Arc<WorkspaceIdentity>, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        self.attach(Some(session), launch)
    }

    /// Capture entry incarnations before the slow filesystem/Git preparation.
    pub fn selection_probe(&self) -> SelectionProbe {
        SelectionProbe {
            entries: self
                .entries
                .iter()
                .map(|(key, entry)| (key.clone(), entry.identity.clone()))
                .collect(),
        }
    }

    /// Synchronous convenience for fixtures. Daemon callers prepare unlocked.
    pub fn select(
        &mut self,
        session: u64,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        let prepared = self.selection_probe().prepare(launch, selected)?;
        self.select_prepared(session, prepared)
    }

    /// Commit prepared membership and Git checks performed outside the registry.
    /// Shared attachment retains its replacement rechecks and bounded retirement wait.
    pub fn select_prepared(
        &mut self,
        session: u64,
        prepared: PreparedSelection,
    ) -> Result<SelectedCheckout, SelectionError> {
        let PreparedSelection { witness, expected } = prepared;
        let key = &witness.root_key;
        let current = self.entries.get(key).map(|entry| &entry.identity);
        if current != expected.as_ref()
            && !current.is_some_and(|identity| identity.matches(&witness.identity))
        {
            return Err(SelectionError::IdentityChanged);
        }
        let identity = self.attach(Some(session), &witness.identity)?;
        Ok(SelectedCheckout {
            identity,
            ..witness
        })
    }

    /// Capacity is a resolved-checkout failure: recheck the selected root
    /// read-only before attributing the error, without reserving a slot.
    pub fn capacity_witness(
        &self,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        Self::capacity_witness_unlocked(launch, selected)
    }

    pub fn capacity_witness_unlocked(
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        Self::capacity_witness_with_common_dirs(launch, selected, |identity| {
            identity
                .git_common_dir()
                .map_err(|_| SelectionError::Unavailable)
        })
    }

    pub(super) fn capacity_witness_with_common_dirs(
        launch: &WorkspaceIdentity,
        selected: &Path,
        mut common_dir: impl FnMut(&WorkspaceIdentity) -> Result<PathBuf, SelectionError>,
    ) -> Result<SelectedCheckout, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        let root = checkout_root(selected)?;
        let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root)
            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
            .map_err(|_| SelectionError::IdentityChanged)?;
        let inode = |path| {
            fs::metadata(path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|_| SelectionError::Unavailable)
        };
        let launch_common = inode(common_dir(launch)?)?;
        let selected_common = inode(common_dir(&identity)?)?;
        let witness = SelectedCheckout {
            launch: Arc::new(
                launch
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            identity: Arc::new(
                identity
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            launch_common,
            selected_common,
        };
        witness.launch.verify_readonly().map_err(identity_fault)?;
        witness.identity.verify_readonly().map_err(identity_fault)?;
        let dirs = CommonDirs {
            launch: common_dir(&witness.launch)?,
            selected: common_dir(&witness.identity)?,
        };
        witness.verify_at_send(&dirs)?;
        Ok(witness)
    }
}

impl SelectedCheckout {
    pub fn before_answer(&self) -> Result<(), SelectionError> {
        self.before_answer_by(None)
    }
    /// `before_answer` whose Git lookups never wait past `by` (#131); one
    /// that cannot finish by then is `Unavailable`.
    pub fn before_answer_by(&self, by: Option<Instant>) -> Result<(), SelectionError> {
        let dirs = self.common_dirs_by(by)?;
        self.verify_at_send(&dirs)
    }
    /// The slow part of the final check (#131): discover both checkouts'
    /// common Git directories, each lookup waiting at most until `by`.
    pub fn common_dirs_by(&self, by: Option<Instant>) -> Result<CommonDirs, SelectionError> {
        self.launch.verify_readonly().map_err(identity_fault)?;
        self.identity.verify_readonly().map_err(identity_fault)?;
        let discover = |identity: &WorkspaceIdentity| {
            identity
                .git_common_dir_by(by)
                .map_err(|_| SelectionError::Unavailable)
        };
        Ok(CommonDirs {
            launch: discover(&self.launch)?,
            selected: discover(&self.identity)?,
        })
    }
    /// The send-time part of the final check (#131), filesystem calls only:
    /// both roots still verify, and the discovered common directories are
    /// still the captured one. A discovered directory that has gone is a
    /// proven change; any other I/O error proves nothing and is `Unavailable`.
    /// A Git pointer re-pointed in place, with every root and
    /// common-directory inode unchanged, is not seen here.
    pub fn verify_at_send(&self, dirs: &CommonDirs) -> Result<(), SelectionError> {
        self.launch.verify_readonly().map_err(identity_fault)?;
        self.identity.verify_readonly().map_err(identity_fault)?;
        let inode = |path: &Path| {
            fs::metadata(path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|error| io_fault(&error))
        };
        if inode(&dirs.launch)? != self.launch_common
            || inode(&dirs.selected)? != self.selected_common
        {
            return Err(SelectionError::IdentityChanged);
        }
        Ok(())
    }
}

/// A final check's failure is a proven change unless it is an I/O error
/// other than a missing path, such as `EACCES` or `EIO` (#131): that proves
/// nothing, so the check is `Unavailable` and runs again.
fn identity_fault(error: anyhow::Error) -> SelectionError {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map_or(SelectionError::IdentityChanged, io_fault)
}

fn io_fault(error: &std::io::Error) -> SelectionError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            SelectionError::IdentityChanged
        }
        _ => SelectionError::Unavailable,
    }
}

fn common_identity(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    common_identity_by(identity, None)
}

fn common_identity_by(
    identity: &WorkspaceIdentity,
    by: Option<Instant>,
) -> Result<(u64, u64), SelectionError> {
    let path = identity
        .git_common_dir_by(by)
        .map_err(|_| SelectionError::Unavailable)?;
    let metadata = fs::metadata(path).map_err(|_| SelectionError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Resolve the selected pathname as T01 discovery does: canonicalize its
/// ancestors once (so `/tmp/x` on macOS selects `/private/tmp/x`, #729), but
/// never follow a final-component symlink into a different root.
fn checkout_root(selected: &Path) -> Result<PathBuf, SelectionError> {
    if !selected.is_absolute() {
        return Err(SelectionError::NotAbsolute);
    }
    if selected
        .components()
        .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(SelectionError::NotCheckout);
    }
    let selected = match (selected.parent(), selected.file_name()) {
        (Some(parent), Some(name)) => fs::canonicalize(parent)
            .map_err(|_| SelectionError::Unavailable)?
            .join(name),
        _ => selected.to_path_buf(),
    };
    let mut literal = PathBuf::new();
    for component in selected.components() {
        literal.push(component);
        let metadata = fs::symlink_metadata(&literal).map_err(|_| SelectionError::Unavailable)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SelectionError::NotCheckout);
        }
    }
    for candidate in literal.ancestors() {
        match fs::symlink_metadata(candidate.join(".git")) {
            Ok(metadata) if metadata.is_dir() || metadata.is_file() => {
                if metadata.file_type().is_symlink() {
                    return Err(SelectionError::NotCheckout);
                }
                return Ok(candidate.to_path_buf());
            }
            Ok(_) => return Err(SelectionError::NotCheckout),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(SelectionError::Unavailable),
        }
    }
    Err(SelectionError::NotCheckout)
}
