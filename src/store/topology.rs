//! Verified filesystem identities and lock primitives for the fixed storage topology.
use anyhow::{Context, Result, bail, ensure};
use directories::ProjectDirs;
use sha2::{Digest, Sha256};
use std::os::unix::{
    fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    io::AsRawFd,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug)]
pub struct IndexNotReady(&'static str);
impl IndexNotReady {
    pub(crate) fn new(reason: &'static str) -> Self {
        Self(reason)
    }
}
impl std::fmt::Display for IndexNotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "index_not_ready: {}", self.0)
    }
}
impl std::error::Error for IndexNotReady {}

fn owner() -> u32 {
    unsafe { libc::geteuid() }
}
fn metadata(path: &Path) -> Result<fs::Metadata> {
    fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))
}
fn private_dir(path: &Path) -> Result<()> {
    let m = metadata(path)?;
    ensure!(
        m.is_dir() && !m.file_type().is_symlink() && m.uid() == owner() && m.mode() & 0o077 == 0,
        "unsafe managed directory: {}",
        path.display()
    );
    Ok(())
}
fn private_file(path: &Path, file: &File) -> Result<()> {
    let m = file.metadata()?;
    let named = metadata(path)?;
    ensure!(
        m.is_file()
            && m.uid() == owner()
            && m.mode() & 0o777 == 0o600
            && m.nlink() == 1
            && named.is_file()
            && !named.file_type().is_symlink()
            && (m.dev(), m.ino()) == (named.dev(), named.ino()),
        "unsafe managed file: {}",
        path.display()
    );
    Ok(())
}
fn open_file_readonly(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    private_file(path, &file)?;
    Ok(file)
}
fn open_file(path: &Path, create: bool) -> Result<File> {
    let mut o = OpenOptions::new();
    o.read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    if create {
        o.create(true);
    }
    let f = o.open(path)?;
    private_file(path, &f)?;
    Ok(f)
}
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))
}
fn make_private(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => sync_directory(path.parent().context("directory has no parent")?)?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    private_dir(path)
}
fn managed_tree(path: &Path) -> Result<()> {
    // Existing system ancestors are not ours; each newly created component is private.
    let mut missing = vec![];
    let mut at = path;
    while fs::symlink_metadata(at).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        missing.push(at.to_owned());
        at = at.parent().context("no existing ancestor")?;
    }
    ensure!(
        !metadata(at)?.file_type().is_symlink() && metadata(at)?.is_dir(),
        "unsafe ancestor: {}",
        at.display()
    );
    for p in missing.iter().rev() {
        make_private(p)?;
    }
    private_dir(path)
}

#[derive(Clone, Debug)]
pub struct TopologyRoots {
    pub cache: PathBuf,
    pub data: PathBuf,
}
impl TopologyRoots {
    pub fn production() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "squashmerge", "trellis")
            .context("ProjectDirs unavailable")?;
        Ok(Self {
            cache: dirs.cache_dir().to_owned(),
            data: dirs.data_local_dir().to_owned(),
        })
    }
    /// Only integration fixtures may inject isolated roots; production uses `production`.
    pub fn isolated_for_tests(cache: PathBuf, data: PathBuf) -> Self {
        Self { cache, data }
    }
    pub fn index_dir(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.cache.join("indexes").join(&identity.root_key)
    }
    pub fn index_use_lock(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.cache
            .join("indexes")
            .join(format!("{}.lock", identity.root_key))
    }
    pub fn index_db(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.index_dir(identity).join("index.db")
    }
    pub fn requests_db(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.index_dir(identity).join("requests.db")
    }
    pub fn record_dir(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.data.join("workspaces").join(&identity.record_id)
    }
    pub fn record_use_lock(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.data
            .join("workspaces")
            .join(format!("{}.lock", identity.record_id))
    }
    pub fn record_db(&self, identity: &WorkspaceIdentity) -> PathBuf {
        self.record_dir(identity).join("workspace.db")
    }
    pub fn reject_root_overlap(&self, identity: &WorkspaceIdentity) -> Result<()> {
        let cache = resolve_existing_ancestor(&self.cache)?;
        let data = resolve_existing_ancestor(&self.data)?;
        ensure!(
            !overlaps(&identity.root, &cache) && !overlaps(&identity.root, &data),
            "workspace root overlaps fixed topology"
        );
        Ok(())
    }
    pub fn prepare_index(&self, identity: &WorkspaceIdentity) -> Result<()> {
        self.reject_root_overlap(identity)?;
        managed_tree(&self.cache)?;
        make_private(&self.cache.join("indexes"))?;
        make_private(&self.index_dir(identity))
    }
    pub fn prepare_records(&self, identity: &WorkspaceIdentity) -> Result<()> {
        self.reject_root_overlap(identity)?;
        managed_tree(&self.data)?;
        make_private(&self.data.join("workspaces"))
    }
    /// Provision directory and use lock during Store activation only.
    pub fn index_use(&self, identity: &WorkspaceIdentity) -> Result<UseGuard> {
        self.prepare_index(identity)?;
        UseGuard::acquire(&self.index_use_lock(identity), false, true)
    }
    pub fn index_use_existing(&self, identity: &WorkspaceIdentity) -> Result<UseGuard> {
        self.index_use_existing_with_wait(identity, true)
    }
    /// Status may hold the existing shared use lock, but must never wait for a
    /// writer or create an index/lock pathname merely to report current state.
    pub fn index_use_existing_readonly(&self, identity: &WorkspaceIdentity) -> Result<UseGuard> {
        identity.verify_readonly()?;
        self.reject_root_overlap(identity)?;
        for path in [
            &self.cache,
            &self.cache.join("indexes"),
            &self.index_dir(identity),
        ] {
            private_dir(path)?;
        }
        UseGuard::acquire_existing_readonly(&self.index_use_lock(identity)).map_err(|error| {
            if error.is::<StorageBusy>() {
                error
            } else {
                error.context("incompatible_index: missing or unsafe use lock")
            }
        })
    }
    /// Inspect existing scratch topology without reacquiring the writer's use lock.
    pub(super) fn existing_index_dir(&self, identity: &WorkspaceIdentity) -> Result<PathBuf> {
        identity.verify()?;
        self.reject_root_overlap(identity)?;
        let directory = self.index_dir(identity);
        for path in [&self.cache, &self.cache.join("indexes"), &directory] {
            private_dir(path)?;
        }
        Ok(directory)
    }
    fn index_use_existing_with_wait(
        &self,
        identity: &WorkspaceIdentity,
        nonblocking: bool,
    ) -> Result<UseGuard> {
        self.existing_index_dir(identity)?;
        UseGuard::acquire_existing(&self.index_use_lock(identity), false, nonblocking).map_err(
            |error| {
                if error.is::<StorageBusy>() {
                    error
                } else {
                    error.context("incompatible_index: missing or unsafe use lock")
                }
            },
        )
    }
    /// Root-loss queue transitions still require the existing protected index directory.
    /// They cannot use the root pathname, which now names a different inode or is absent.
    pub(crate) fn index_use_existing_without_root(
        &self,
        identity: &WorkspaceIdentity,
    ) -> Result<UseGuard> {
        for path in [
            &self.cache,
            &self.cache.join("indexes"),
            &self.index_dir(identity),
        ] {
            private_dir(path)?;
        }
        UseGuard::acquire_existing(&self.index_use_lock(identity), false, true)
    }
    /// Exceptional index replacement starts only after every protected handle closes.
    /// Never create or upgrade a use lock while attempting exclusive admission.
    pub fn index_use_exclusive_existing(&self, identity: &WorkspaceIdentity) -> Result<UseGuard> {
        identity.verify()?;
        self.reject_root_overlap(identity)?;
        for path in [
            &self.cache,
            &self.cache.join("indexes"),
            &self.index_dir(identity),
        ] {
            private_dir(path)?;
        }
        UseGuard::acquire_existing(&self.index_use_lock(identity), true, true)
    }
    /// Admit a daemon-local writer without electing among processes. The shared
    /// use lock stays held for the writer's lifetime; exclusive GC cannot unlink
    /// an index while the writer uses it.
    pub fn writer(&self, identity: Arc<WorkspaceIdentity>) -> Result<WriterGuard> {
        let use_guard = self.index_use_existing(&identity)?;
        WriterGuard::new(self, identity, use_guard, false)
    }
    /// Activation retains the existing exclusive use lock through publication.
    pub fn writer_under_exclusive(
        &self,
        identity: Arc<WorkspaceIdentity>,
        use_guard: UseGuard,
    ) -> Result<WriterGuard> {
        WriterGuard::new(self, identity, use_guard, true)
    }
    pub fn record_use(&self, identity: &WorkspaceIdentity, exclusive: bool) -> Result<UseGuard> {
        self.prepare_records(identity)?;
        UseGuard::acquire(&self.record_use_lock(identity), exclusive, exclusive)
    }

    pub fn validate_external(
        &self,
        identity: &WorkspaceIdentity,
        destinations: &[PathBuf],
    ) -> Result<()> {
        let protected = [
            identity.root.clone(),
            resolve_existing_ancestor(&self.cache)?,
            resolve_existing_ancestor(&self.data)?,
        ];
        let resolved: Vec<PathBuf> = destinations
            .iter()
            .map(|p| resolve_existing_ancestor(p))
            .collect::<Result<_>>()?;
        for target in &resolved {
            ensure!(
                !protected.iter().any(|p| overlaps(target, p)),
                "external destination overlaps checkout or topology: {}",
                target.display()
            );
            for parent in target.ancestors() {
                if parent.join(".git").symlink_metadata().is_ok() {
                    bail!(
                        "external destination inside Git checkout: {}",
                        target.display()
                    );
                }
            }
        }
        for (i, a) in resolved.iter().enumerate() {
            ensure!(
                !resolved[i + 1..].iter().any(|b| overlaps(a, b)),
                "external destinations overlap"
            );
        }
        Ok(())
    }
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}
fn resolve_existing_ancestor(path: &Path) -> Result<PathBuf> {
    ensure!(path.is_absolute(), "external path must be absolute");
    let mut rest = vec![];
    let mut p = path;
    while fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
        rest.push(
            p.file_name()
                .context("invalid external path")?
                .to_os_string(),
        );
        p = p.parent().context("invalid external path")?;
    }
    let mut out = fs::canonicalize(p)?;
    for c in rest.iter().rev() {
        match Path::new(c).components().next() {
            Some(Component::ParentDir) => {
                out.pop();
            }
            Some(Component::CurDir) => {}
            _ => out.push(c),
        }
    }
    Ok(out)
}

#[derive(Debug)]
pub struct WorkspaceIdentity {
    pub root: PathBuf,
    pub root_key: String,
    pub record_id: String,
    pub device: u64,
    pub inode: u64,
    git_dir: Option<PathBuf>,
    marker: Option<Uuid>,
    root_handle: File,
}
impl WorkspaceIdentity {
    pub fn discover(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        Self::discover_with_marker_hook(explicit, cwd, |_| Ok(()))
    }
    /// Fixture hook to inject a failure immediately before marker descriptor fsync.
    pub fn discover_with_marker_sync_hook(
        explicit: Option<&Path>,
        cwd: &Path,
        before_sync: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        let mut before_sync = Some(before_sync);
        Self::discover_with_marker_hook(explicit, cwd, |stage| {
            if stage == MarkerStage::MarkerSync {
                before_sync.take().context("marker sync hook reused")?()
            } else {
                Ok(())
            }
        })
    }
    /// Fixture-only interleaving and durability hook; ordinary discovery supplies no barrier.
    pub fn discover_with_marker_hook(
        explicit: Option<&Path>,
        cwd: &Path,
        mut hook: impl FnMut(MarkerStage) -> Result<()>,
    ) -> Result<Self> {
        Self::discover_unattached(explicit, cwd)?.attach_marker_with_hook(&mut hook)
    }
    /// Inspect identity without creating the Git marker, for external destination preflight.
    pub fn discover_unattached(explicit: Option<&Path>, cwd: &Path) -> Result<Self> {
        let selected = if let Some(p) = explicit {
            p.to_owned()
        } else {
            let cwd = fs::canonicalize(cwd)?;
            let mut found = None;
            for p in cwd.ancestors() {
                match fs::symlink_metadata(p.join(".git")) {
                    Ok(_) => {
                        found = Some(p.to_owned());
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            found.unwrap_or(cwd)
        };
        ensure!(
            !metadata(&selected)?.file_type().is_symlink(),
            "root_changed: final root symlink"
        );
        let root = fs::canonicalize(selected)?;
        ensure!(
            root.is_absolute() && root.to_str().is_some() && metadata(&root)?.is_dir(),
            "invalid workspace root"
        );
        if explicit.is_none() {
            ensure!(
                root.parent().is_some() && Some(root.as_path()) != dirs_home().as_deref(),
                "implicit home or filesystem root refused"
            );
        }
        let root_handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&root)?;
        let m = root_handle.metadata()?;
        let spelling = root.to_str().context("non-UTF8 workspace")?;
        let spelling = if spelling == "/" {
            spelling
        } else {
            spelling.trim_end_matches('/')
        };
        let root_key = hex::encode(Sha256::digest(spelling.as_bytes()));
        let git_dir = match fs::symlink_metadata(root.join(".git")) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Some(root.join(".git")),
            Ok(m) if m.is_file() && !m.file_type().is_symlink() => {
                Some(parse_git_pointer(&root.join(".git"))?)
            }
            Ok(_) => bail!("unsafe .git entry"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let record_id = format!("path-{root_key}");
        Ok(Self {
            root,
            root_key,
            record_id,
            device: m.dev(),
            inode: m.ino(),
            git_dir,
            marker: None,
            root_handle,
        })
    }
    pub fn attach_marker(self) -> Result<Self> {
        self.attach_marker_with_hook(&mut |_| Ok(()))
    }
    /// Status observes an existing Git workspace identity without creating or
    /// fsyncing the marker. A missing marker means no published workspace.
    pub fn attach_existing_marker_readonly(mut self) -> Result<Self> {
        if let Some(git) = &self.git_dir {
            ensure!(
                metadata(git)?.uid() == owner() && metadata(git)?.is_dir(),
                "unsafe Git directory"
            );
            let path = git.join("trellis/workspace-id");
            let marker = read_marker_readonly(&path).map_err(|error| {
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                {
                    anyhow::anyhow!("index_not_ready: no published workspace identity")
                } else {
                    error
                }
            })?;
            self.record_id = marker.to_string();
            self.marker = Some(marker);
        }
        self.verify_readonly()?;
        Ok(self)
    }
    fn attach_marker_with_hook(
        mut self,
        hook: &mut impl FnMut(MarkerStage) -> Result<()>,
    ) -> Result<Self> {
        let current = metadata(&self.root).context("root_changed")?;
        let captured = self.root_handle.metadata()?;
        ensure!(
            current.is_dir()
                && !current.file_type().is_symlink()
                && (current.dev(), current.ino()) == (self.device, self.inode)
                && (captured.dev(), captured.ino()) == (self.device, self.inode),
            "root_changed"
        );
        if let Some(git) = &self.git_dir {
            ensure!(
                resolve_git_dir(&self.root)?.as_deref() == Some(git),
                "workspace_id_changed"
            );
            let marker = marker_at(git, hook)?;
            self.record_id = marker.to_string();
            self.marker = Some(marker);
        }
        self.verify()?;
        Ok(self)
    }
    /// Only a proven pathname loss authorizes old-root queue failure. A changed
    /// workspace marker or an unreadable pathname is not proof of replacement.
    pub(crate) fn root_path_replaced(&self) -> Result<bool> {
        let held = self.root_handle.metadata()?;
        ensure!(
            (held.dev(), held.ino()) == (self.device, self.inode),
            "root_changed: captured handle identity changed"
        );
        self.root_path_replaced_from(fs::symlink_metadata(&self.root))
    }
    fn root_path_replaced_from(&self, named: std::io::Result<fs::Metadata>) -> Result<bool> {
        match named {
            Ok(m) => Ok(!m.is_dir()
                || m.file_type().is_symlink()
                || (m.dev(), m.ino()) != (self.device, self.inode)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(error) => Err(error.into()),
        }
    }
    pub fn verify(&self) -> Result<()> {
        self.verify_with_marker(false)
    }
    /// Recheck the same open root and marker without discovering or creating a
    /// different identity when another owner repaired a pending derived index.
    pub(crate) fn verified_clone(&self) -> Result<Self> {
        self.verify()?;
        Ok(Self {
            root: self.root.clone(),
            root_key: self.root_key.clone(),
            record_id: self.record_id.clone(),
            device: self.device,
            inode: self.inode,
            git_dir: self.git_dir.clone(),
            marker: self.marker,
            root_handle: self.root_handle.try_clone()?,
        })
    }

    #[cfg(feature = "test-support")]
    pub fn verified_clone_for_tests(&self) -> Result<Self> {
        self.verified_clone()
    }
    pub fn verify_readonly(&self) -> Result<()> {
        self.verify_with_marker(true)
    }
    fn verify_with_marker(&self, readonly: bool) -> Result<()> {
        let m = fs::symlink_metadata(&self.root).context("root_changed")?;
        let handle = self.root_handle.metadata()?;
        ensure!(
            m.is_dir()
                && !m.file_type().is_symlink()
                && (m.dev(), m.ino()) == (self.device, self.inode)
                && (handle.dev(), handle.ino()) == (self.device, self.inode),
            "root_changed"
        );
        if let Some(git) = &self.git_dir {
            ensure!(
                resolve_git_dir(&self.root)?.as_deref() == Some(git),
                "workspace_id_changed"
            );
            let marker = if readonly {
                read_marker_readonly(&git.join("trellis/workspace-id"))?
            } else {
                read_marker(&git.join("trellis/workspace-id"))?
            };
            ensure!(Some(marker) == self.marker, "workspace_id_changed");
        }
        Ok(())
    }
    /// Resolve the common Git directory without creating any managed storage.
    /// Both the held root and its pathname must still designate this checkout.
    pub fn git_common_dir(&self) -> Result<PathBuf> {
        self.git_common_dir_by(None)
    }
    /// The lookup waits at most its own bound, and never past `by` (#131):
    /// a caller's deadline caps the wait, and reaching either is `unavailable`.
    pub fn git_common_dir_by(&self, by: Option<std::time::Instant>) -> Result<PathBuf> {
        self.git_common_dir_with_executable_by(Path::new("git"), by)
    }
    /// Alternate executable permits a deterministic stalled-child fixture.
    #[doc(hidden)]
    pub fn git_common_dir_with_executable(&self, executable: &Path) -> Result<PathBuf> {
        self.git_common_dir_with_executable_by(executable, None)
    }
    fn git_common_dir_with_executable_by(
        &self,
        executable: &Path,
        by: Option<std::time::Instant>,
    ) -> Result<PathBuf> {
        self.verify_readonly()?;
        ensure!(
            self.git_dir.is_some(),
            "not_checkout: missing Git directory"
        );
        let bound = std::time::Instant::now() + Duration::from_millis(750);
        let deadline = by.map_or(bound, |by| by.min(bound));
        ensure!(
            std::time::Instant::now() < deadline,
            "unavailable: Git common-directory lookup timed out"
        );
        let mut child = std::process::Command::new(executable)
            .arg("-C")
            .arg(&self.root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("unavailable: start Git common-directory lookup")?;
        let stdout = child.stdout.take().context("missing Git output")?;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let reader = std::thread::spawn(move || {
            let mut output = Vec::new();
            let result = stdout.take(4097).read_to_end(&mut output);
            let _ = sender.send(result.map(|_| output));
        });
        let output = match receiver
            .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(result) => result,
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                // A shell wrapper's descendant can still hold the inherited pipe.
                // Drop the join handle: its bounded reader exits when that pipe closes.
                drop(reader);
                bail!("unavailable: Git common-directory lookup timed out");
            }
        };
        if output.as_ref().is_ok_and(|bytes| bytes.len() > 4096) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            bail!("unavailable: Git common directory exceeds path limit");
        }
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("unavailable: Git output reader failed"))?;
        let output = output.context("unavailable: read Git common directory")?;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("unavailable: Git common-directory lookup timed out");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        ensure!(
            status.success(),
            "not_checkout: Git common directory unavailable"
        );
        let text = std::str::from_utf8(&output)?.trim_end_matches('\n');
        ensure!(
            !text.contains(['\r', '\n', '\0']),
            "invalid Git common directory"
        );
        let path = PathBuf::from(text);
        ensure!(path.is_absolute(), "invalid Git common directory");
        let path = fs::canonicalize(path)?;
        ensure!(metadata(&path)?.is_dir(), "invalid Git common directory");
        self.verify_readonly()?;
        Ok(path)
    }
    pub fn git_dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }
}
fn dirs_home() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_owned())
}
fn resolve_git_dir(root: &Path) -> Result<Option<PathBuf>> {
    let p = root.join(".git");
    match metadata(&p) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(Some(p)),
        Ok(m) if m.is_file() && !m.file_type().is_symlink() => Ok(Some(parse_git_pointer(&p)?)),
        Ok(_) => bail!("unsafe .git entry"),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
fn parse_git_pointer(path: &Path) -> Result<PathBuf> {
    let m = metadata(path)?;
    ensure!(
        m.uid() == owner() && m.nlink() == 1 && m.len() <= 4096,
        "unsafe .git pointer"
    );
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file()
            && opened.uid() == owner()
            && opened.nlink() == 1
            && (opened.dev(), opened.ino()) == (m.dev(), m.ino()),
        "unsafe .git pointer"
    );
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 4096 && !bytes.contains(&0),
        "invalid .git pointer"
    );
    let text = std::str::from_utf8(&bytes)?;
    let value = text
        .strip_prefix("gitdir: ")
        .context("invalid .git pointer")?;
    let value = value
        .strip_suffix("\r\n")
        .or_else(|| value.strip_suffix('\n'))
        .unwrap_or(value);
    ensure!(
        !value.is_empty() && value.trim() == value && !value.contains(['\r', '\n']),
        "invalid .git pointer"
    );
    let target = path.parent().unwrap().join(value);
    let mut normalized = PathBuf::new();
    for component in target.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component)
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    ensure!(normalized.is_absolute(), "invalid .git pointer target");
    for p in normalized.ancestors().collect::<Vec<_>>().iter().rev() {
        let m = metadata(p)?;
        ensure!(
            !m.file_type().is_symlink(),
            "symlink Git directory traversal"
        );
    }
    let m = metadata(&normalized)?;
    ensure!(m.is_dir() && m.uid() == owner(), "unsafe Git directory");
    Ok(normalized)
}
/// Stages exposed only to integration fixtures; no production behavior depends on a hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerStage {
    CreatedBeforeWrite,
    ShortRead,
    ShortRechecked,
    MarkerSync,
    PrivateDirSync,
    GitDirSync,
}

enum MarkerRead {
    Valid(Uuid, File),
    Short(File),
}
fn read_marker_file(path: &Path) -> Result<MarkerRead> {
    classify_marker_file(path, open_file(path, false)?)
}
fn classify_marker_file(path: &Path, f: File) -> Result<MarkerRead> {
    let mut data = Vec::new();
    (&f).take(37).read_to_end(&mut data)?;
    private_file(path, &f)?;
    if data.len() < 36 {
        return Ok(MarkerRead::Short(f));
    }
    ensure!(data.len() == 36, "invalid workspace-id marker");
    let text = std::str::from_utf8(&data)?;
    let id = Uuid::parse_str(text)?;
    ensure!(
        !id.is_nil() && id.get_version_num() == 4 && id.to_string() == text,
        "invalid workspace-id marker"
    );
    Ok(MarkerRead::Valid(id, f))
}
fn read_marker_readonly(path: &Path) -> Result<Uuid> {
    match classify_marker_file(path, open_file_readonly(path)?)? {
        MarkerRead::Valid(id, _) => Ok(id),
        MarkerRead::Short(_) => bail!("invalid workspace-id marker"),
    }
}
fn read_marker(path: &Path) -> Result<Uuid> {
    match read_marker_file(path)? {
        MarkerRead::Valid(id, _) => Ok(id),
        MarkerRead::Short(_) => bail!("invalid workspace-id marker"),
    }
}
fn read_marker_during_creation(
    path: &Path,
    first: MarkerRead,
    hook: &mut impl FnMut(MarkerStage) -> Result<()>,
) -> Result<(Uuid, File)> {
    let mut current = first;
    let mut original_short: Option<File> = None;
    for attempt in 0..20 {
        match current {
            MarkerRead::Valid(id, file) => return Ok((id, file)),
            MarkerRead::Short(file) => {
                hook(MarkerStage::ShortRead)?;
                private_file(path, &file)?;
                if attempt == 19 {
                    bail!("invalid workspace-id marker: persistently short");
                }
                if original_short.is_none() {
                    original_short = Some(file);
                }
                hook(MarkerStage::ShortRechecked)?;
                std::thread::sleep(Duration::from_millis(5));
                let reopened = read_marker_file(path)?;
                let reopened_file = match &reopened {
                    MarkerRead::Valid(_, file) | MarkerRead::Short(file) => file,
                };
                let original = original_short.as_ref().expect("short marker was observed");
                let before = original.metadata()?;
                let after = reopened_file.metadata()?;
                ensure!(
                    (before.dev(), before.ino()) == (after.dev(), after.ino()),
                    "workspace-id marker changed during retry"
                );
                current = reopened;
            }
        }
    }
    unreachable!()
}
fn durable_marker(
    path: &Path,
    trellis: &Path,
    git: &Path,
    file: &File,
    hook: &mut impl FnMut(MarkerStage) -> Result<()>,
) -> Result<()> {
    private_file(path, file).context("workspace_id_not_durable")?;
    hook(MarkerStage::MarkerSync).context("workspace_id_not_durable")?;
    file.sync_all().context("workspace_id_not_durable")?;
    private_file(path, file).context("workspace_id_not_durable")?;
    hook(MarkerStage::PrivateDirSync).context("workspace_id_not_durable")?;
    sync_directory(trellis).context("workspace_id_not_durable")?;
    private_file(path, file).context("workspace_id_not_durable")?;
    hook(MarkerStage::GitDirSync).context("workspace_id_not_durable")?;
    sync_directory(git).context("workspace_id_not_durable")?;
    private_file(path, file).context("workspace_id_not_durable")
}
fn marker_at(git: &Path, hook: &mut impl FnMut(MarkerStage) -> Result<()>) -> Result<Uuid> {
    ensure!(
        metadata(git)?.uid() == owner() && metadata(git)?.is_dir(),
        "unsafe Git directory"
    );
    let trellis = git.join("trellis");
    make_private(&trellis)?;
    let path = trellis.join("workspace-id");
    // Only an absent initial pathname may start a new UUID. Once opened,
    // descriptor/path failures (including NotFound) must not regenerate it.
    let initial = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path);
    match initial {
        Ok(file) => {
            let first = classify_marker_file(&path, file)?;
            let (id, file) = read_marker_during_creation(&path, first, hook)?;
            durable_marker(&path, &trellis, git, &file, hook)?;
            Ok(id)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = Uuid::new_v4();
            let created = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path);
            match created {
                Ok(mut f) => {
                    private_file(&path, &f)?;
                    hook(MarkerStage::CreatedBeforeWrite)?;
                    f.write_all(id.to_string().as_bytes())?;
                    durable_marker(&path, &trellis, git, &f, hook)?;
                    Ok(id)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let first = read_marker_file(&path)?;
                    let (id, file) = read_marker_during_creation(&path, first, hook)?;
                    durable_marker(&path, &trellis, git, &file, hook)?;
                    Ok(id)
                }
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}
#[derive(Debug)]
pub(crate) struct StorageBusy;
impl std::fmt::Display for StorageBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage_busy")
    }
}
impl std::error::Error for StorageBusy {}

#[derive(Debug)]
pub struct UseGuard {
    file: File,
    path: PathBuf,
    exclusive: bool,
}
impl UseGuard {
    pub fn acquire(path: &Path, exclusive: bool, nonblocking: bool) -> Result<Self> {
        Self::acquire_with_hook(path, exclusive, nonblocking, || Ok(()))
    }
    /// Fixture barrier after the first inode is opened, before flock.
    pub fn acquire_with_hook(
        path: &Path,
        exclusive: bool,
        nonblocking: bool,
        after_open: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        Self::acquire_mode(path, exclusive, nonblocking, true, false, after_open)
    }
    pub fn acquire_existing_with_hook(
        path: &Path,
        exclusive: bool,
        nonblocking: bool,
        after_open: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        Self::acquire_mode(path, exclusive, nonblocking, false, false, after_open)
    }
    pub fn acquire_existing(path: &Path, exclusive: bool, nonblocking: bool) -> Result<Self> {
        Self::acquire_existing_with_hook(path, exclusive, nonblocking, || Ok(()))
    }
    pub fn acquire_existing_readonly(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, false, true, false, true, || Ok(()))
    }
    pub fn acquire_existing_readonly_exclusive(path: &Path) -> Result<Self> {
        Self::acquire_mode(path, true, true, false, true, || Ok(()))
    }
    fn acquire_mode(
        path: &Path,
        exclusive: bool,
        nonblocking: bool,
        create: bool,
        readonly: bool,
        after_open: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        let flags = (if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        }) | (if nonblocking { libc::LOCK_NB } else { 0 });
        let mut hook = Some(after_open);
        ensure!(
            !readonly || !create,
            "read-only use lock cannot create a pathname"
        );
        for _ in 0..20 {
            let file = if readonly {
                open_file_readonly(path)?
            } else {
                open_file(path, create)?
            };
            if let Some(after_open) = hook.take() {
                after_open()?;
            }
            let status = unsafe { libc::flock(file.as_raw_fd(), flags) };
            if status != 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    return Err(StorageBusy.into());
                }
                return Err(e.into());
            }
            let named = fs::symlink_metadata(path);
            if let Ok(m) = &named {
                let fd = file.metadata()?;
                if (m.dev(), m.ino()) == (fd.dev(), fd.ino()) {
                    private_file(path, &file)?;
                    return Ok(Self {
                        file,
                        path: path.to_owned(),
                        exclusive,
                    });
                }
            } else if !named
                .as_ref()
                .err()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                named?;
            }
        }
        bail!("lock pathname changed repeatedly: {}", path.display())
    }
    pub fn verify(&self) -> Result<()> {
        private_file(&self.path, &self.file)
    }
    fn belongs_to(&self, path: &Path, exclusive: bool) -> Result<()> {
        ensure!(
            self.path == path && self.exclusive == exclusive,
            "unsafe_index: wrong index use lock path or mode"
        );
        self.verify()
    }
    pub(crate) fn verify_exclusive_path(&self, path: &Path) -> Result<()> {
        self.belongs_to(path, true)
    }
    fn downgrade_to_shared(&mut self) -> Result<()> {
        ensure!(
            self.exclusive,
            "unsafe_index: exclusive use lock required for downgrade"
        );
        self.verify()?;
        let status = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.exclusive = false;
        self.verify()
    }
    pub fn remove_last(self) -> Result<()> {
        ensure!(self.exclusive, "exclusive use lock required for removal");
        self.verify()?;
        fs::remove_file(&self.path)?;
        sync_directory(self.path.parent().context("lock parent missing")?)
    }
}
impl Drop for UseGuard {
    fn drop(&mut self) {
        unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A daemon-local writer admission. This is not a cross-process election:
/// concurrent daemons coordinate mutations through SQLite and retained use locks.
#[derive(Debug)]
pub struct WriterGuard {
    identity: Arc<WorkspaceIdentity>,
    use_guard: UseGuard,
    /// An ephemeral request-claim token, never persisted in an election file.
    pub incarnation: Uuid,
}
impl WriterGuard {
    fn new(
        roots: &TopologyRoots,
        identity: Arc<WorkspaceIdentity>,
        use_guard: UseGuard,
        exclusive: bool,
    ) -> Result<Self> {
        identity.verify()?;
        use_guard.belongs_to(&roots.index_use_lock(&identity), exclusive)?;
        Ok(Self {
            identity,
            use_guard,
            incarnation: Uuid::new_v4(),
        })
    }
    pub fn verify(&self) -> Result<()> {
        self.identity.verify()?;
        self.use_guard.verify()
    }
    pub fn belongs_to(&self, identity: &WorkspaceIdentity) -> Result<()> {
        identity.verify()?;
        self.verify()?;
        ensure!(
            self.identity.root == identity.root
                && self.identity.root_key == identity.root_key
                && self.identity.record_id == identity.record_id
                && self.identity.device == identity.device
                && self.identity.inode == identity.inode,
            "storage_busy: writer belongs to another workspace"
        );
        self.use_guard.verify()
    }
    pub fn verify_exclusive_use(&self, use_path: &Path) -> Result<()> {
        self.verify()?;
        self.use_guard.belongs_to(use_path, true)
    }
    pub(crate) fn exclusive_use_guard(&self, use_path: &Path) -> Result<&UseGuard> {
        self.verify_exclusive_use(use_path)?;
        Ok(&self.use_guard)
    }
    pub fn downgrade_use_to_shared(&mut self) -> Result<()> {
        self.verify()?;
        self.use_guard.downgrade_to_shared()?;
        self.verify()
    }
}

#[derive(Debug)]
pub struct WriterSession {
    guard: WriterGuard,
}
impl WriterSession {
    pub fn writer(guard: WriterGuard) -> Self {
        Self { guard }
    }
    pub fn writer_guard(&self) -> Result<&WriterGuard> {
        self.verify()?;
        Ok(&self.guard)
    }
    pub fn incarnation(&self) -> Uuid {
        self.guard.incarnation
    }
    pub fn verify(&self) -> Result<()> {
        self.guard.verify()
    }
    pub fn belongs_to(&self, identity: &WorkspaceIdentity) -> Result<()> {
        self.guard.belongs_to(identity)
    }
}

/// Lazy, versioned durable payloads. The legacy `Store::open` is replaced by the
/// topology cutover; this engine is independently usable until that cutover.
pub struct DurableRecords<'a> {
    roots: &'a TopologyRoots,
    identity: &'a WorkspaceIdentity,
}

const RECORD_SCHEMA: &str = "
CREATE TABLE record_metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=1), record_id TEXT NOT NULL, initialized INTEGER NOT NULL CHECK(initialized=1));
CREATE TABLE known_roots(path TEXT PRIMARY KEY, device TEXT NOT NULL, inode TEXT NOT NULL);
CREATE TABLE views(id TEXT PRIMARY KEY,payload TEXT NOT NULL);
CREATE TABLE annotations(id TEXT PRIMARY KEY,node_id TEXT NOT NULL,payload TEXT NOT NULL);
";

#[derive(Clone, Copy)]
struct RecordItem<'a> {
    table: &'a str,
    id: &'a str,
    node: Option<&'a str>,
}

impl<'a> DurableRecords<'a> {
    pub fn new(roots: &'a TopologyRoots, identity: &'a WorkspaceIdentity) -> Self {
        Self { roots, identity }
    }
    fn existing(&self) -> Result<bool> {
        self.identity.verify()?;
        self.roots.reject_root_overlap(self.identity)?;
        for parent in [&self.roots.data, &self.roots.data.join("workspaces")] {
            match fs::symlink_metadata(parent) {
                Ok(_) => private_dir(parent)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        }
        match fs::symlink_metadata(self.roots.record_dir(self.identity)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
            Ok(_) => {
                private_dir(&self.roots.data)?;
                private_dir(&self.roots.data.join("workspaces"))?;
                private_dir(&self.roots.record_dir(self.identity))?;
                Ok(true)
            }
        }
    }
    fn lock_existing(&self) -> Result<UseGuard> {
        UseGuard::acquire_existing(&self.roots.record_use_lock(self.identity), false, false)
            .context("incomplete_record: missing or unsafe use lock")
    }
    fn lock_existing_writer(&self) -> Result<UseGuard> {
        UseGuard::acquire_existing(&self.roots.record_use_lock(self.identity), true, true)
    }
    fn db(&self, writable: bool) -> Result<rusqlite::Connection> {
        use rusqlite::{Connection, OpenFlags};
        let path = self.roots.record_db(self.identity);
        ensure!(path.exists(), "incomplete_record: missing database");
        let file = open_file(&path, false)?;
        private_file(&path, &file)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = path.with_file_name(format!("workspace.db{suffix}"));
            match fs::symlink_metadata(&sidecar) {
                Ok(_) => bail!("incomplete_record: recovery required"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let flags = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let db = Connection::open_with_flags(&path, flags)?;
        private_file(&path, &file).context("incomplete_record: database changed")?;
        db.busy_timeout(Duration::ZERO)?;
        db.pragma_update(None, "temp_store", "MEMORY")?;
        if writable {
            db.pragma_update(None, "synchronous", "FULL")?;
        } else {
            db.pragma_update(None, "query_only", "ON")?;
        }
        let journal: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        ensure!(journal == "delete", "incompatible_record: journal mode");
        let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version != 0, "incomplete_record: schema not committed");
        ensure!(version == 1, "incompatible_record: schema version");
        let row: (i64, String, i64) = db.query_row("SELECT schema_version,record_id,initialized FROM record_metadata WHERE singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).context("incomplete_record: metadata")?;
        ensure!(
            row == (1, self.identity.record_id.clone(), 1),
            "incomplete_record: metadata mismatch"
        );
        db.prepare("SELECT path,device,inode FROM known_roots")?;
        db.prepare("SELECT id,payload FROM views")?;
        db.prepare("SELECT id,node_id,payload FROM annotations")?;
        self.identity.verify()?;
        Ok(db)
    }
    fn save(&self, item: RecordItem<'_>, payload: String, preserve_title: bool) -> Result<String> {
        self.save_with_first_save_hook(item, payload, preserve_title, |_| Ok(None), |_| Ok(()))
    }
    fn save_with_first_save_hook(
        &self,
        item: RecordItem<'_>,
        payload: String,
        preserve_title: bool,
        mut capture: impl FnMut(&str) -> Result<Option<Box<serde_json::value::RawValue>>>,
        mut hook: impl FnMut(&str) -> Result<()>,
    ) -> Result<String> {
        use rusqlite::{Connection, TransactionBehavior};
        let RecordItem { table, id, node } = item;
        let exists = self.existing()?;
        if !exists {
            self.roots.prepare_records(self.identity)?;
            let guard = self.roots.record_use(self.identity, true)?;
            self.identity.verify()?;
            // Another process may have completed creation before the lock was acquired.
            if self.roots.record_dir(self.identity).exists() {
                drop(guard);
                return self.save_existing(table, id, node, &payload, preserve_title, &mut capture);
            }
            make_private(&self.roots.record_dir(self.identity))?;
            let path = self.roots.record_db(self.identity);
            let _file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            let mut db = Connection::open(&path)?;
            db.busy_timeout(Duration::ZERO)?;
            db.pragma_update(None, "journal_mode", "DELETE")?;
            db.pragma_update(None, "synchronous", "FULL")?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(RECORD_SCHEMA)?;
            tx.pragma_update(None, "user_version", 1)?;
            tx.execute(
                "INSERT INTO record_metadata VALUES(1,1,?1,1)",
                [&self.identity.record_id],
            )?;
            let payload = Self::merge_item(&tx, table, &payload, preserve_title, &mut capture)?;
            Self::write_item(&tx, table, id, node, &payload)?;
            self.write_root(&tx)?;
            self.identity.verify()?;
            guard.verify()?;
            hook("before_commit")?;
            tx.commit()?;
            drop(db);
            hook("before_sync")?;
            open_file(&path, false)?.sync_all()?;
            sync_directory(&self.roots.record_dir(self.identity))?;
            sync_directory(&self.roots.data.join("workspaces"))?;
            sync_directory(&self.roots.data)?;
            return Ok(payload);
        }
        self.save_existing(table, id, node, &payload, preserve_title, &mut capture)
    }
    fn save_existing(
        &self,
        table: &str,
        id: &str,
        node: Option<&str>,
        payload: &str,
        preserve_title: bool,
        capture: &mut impl FnMut(&str) -> Result<Option<Box<serde_json::value::RawValue>>>,
    ) -> Result<String> {
        let guard = self.lock_existing_writer()?;
        let mut db = self.db(true)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let payload = Self::merge_item(&tx, table, payload, preserve_title, capture)?;
        Self::write_item(&tx, table, id, node, &payload)?;
        self.write_root(&tx)?;
        self.identity.verify()?;
        guard.verify()?;
        tx.commit()?;
        drop(db);
        Ok(payload)
    }
    fn merge_item(
        db: &rusqlite::Transaction<'_>,
        table: &str,
        payload: &str,
        preserve_title: bool,
        capture: &mut impl FnMut(&str) -> Result<Option<Box<serde_json::value::RawValue>>>,
    ) -> Result<String> {
        use rusqlite::OptionalExtension;
        if table == "views" {
            let mut incoming: crate::model::SavedViewRecord = serde_json::from_str(payload)?;
            incoming.validate()?;
            if let Some(old) = db
                .query_row(
                    "SELECT payload FROM views WHERE id=?1",
                    [&incoming.id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                let old: crate::model::SavedViewRecord = serde_json::from_str(&old)?;
                old.validate()?;
                ensure!(
                    old.query.seed == incoming.query.seed,
                    "saved view target replacement is not allowed"
                );
                incoming.anchor = old.anchor;
            } else if incoming.anchor.is_none() {
                incoming.anchor = capture("view")?;
            }
            incoming.validate()?;
            Ok(serde_json::to_string(&incoming)?)
        } else {
            let mut incoming: crate::model::AnnotationRecord = serde_json::from_str(payload)?;
            incoming.validate()?;
            if let Some(old) = db
                .query_row(
                    "SELECT payload FROM annotations WHERE id=?1",
                    [&incoming.id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            {
                let old: crate::model::AnnotationRecord = serde_json::from_str(&old)?;
                old.validate()?;
                ensure!(
                    old.node_id == incoming.node_id,
                    "saved annotation target replacement is not allowed"
                );
                incoming.anchor = old.anchor;
                if preserve_title && incoming.title.is_none() {
                    incoming.title = old.title;
                }
            } else if incoming.anchor.is_none() {
                incoming.anchor = capture("annotation")?;
            }
            incoming.validate()?;
            Ok(serde_json::to_string(&incoming)?)
        }
    }

    fn write_item(
        db: &rusqlite::Transaction<'_>,
        table: &str,
        id: &str,
        node: Option<&str>,
        payload: &str,
    ) -> Result<()> {
        use rusqlite::params;
        if table == "views" {
            db.execute("INSERT INTO views VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![id,payload])?;
        } else {
            db.execute("INSERT INTO annotations VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET node_id=excluded.node_id,payload=excluded.payload", params![id,node,payload])?;
        }
        Ok(())
    }
    fn write_root(&self, db: &rusqlite::Transaction<'_>) -> Result<()> {
        db.execute("INSERT INTO known_roots VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET device=excluded.device,inode=excluded.inode", rusqlite::params![self.identity.root.to_str().context("non-UTF8 root")?,self.identity.device.to_string(),self.identity.inode.to_string()])?;
        Ok(())
    }
    fn list<T: serde::de::DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        if !self.existing()? {
            return Ok(vec![]);
        }
        let guard = self.lock_existing()?;
        let mut db = self.db(false)?;
        let tx = db.transaction()?;
        let values = tx
            .prepare(sql)?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let result = values
            .iter()
            .map(|value| serde_json::from_str(value).map_err(Into::into))
            .collect();
        self.identity.verify()?;
        guard.verify()?;
        result
    }
    pub fn view_records(&self) -> Result<Vec<crate::model::SavedViewRecord>> {
        let records: Vec<crate::model::SavedViewRecord> =
            self.list("SELECT payload FROM views ORDER BY id")?;
        for record in &records {
            record.validate()?;
        }
        Ok(records)
    }
    pub fn annotation_records(&self) -> Result<Vec<crate::model::AnnotationRecord>> {
        let records: Vec<crate::model::AnnotationRecord> =
            self.list("SELECT payload FROM annotations ORDER BY id")?;
        for record in &records {
            record.validate()?;
        }
        Ok(records)
    }
    pub fn views(&self) -> Result<Vec<crate::model::SavedView>> {
        Ok(self
            .view_records()?
            .into_iter()
            .map(|record| record.base())
            .collect())
    }
    pub fn annotations(&self) -> Result<Vec<crate::model::Annotation>> {
        Ok(self
            .annotation_records()?
            .into_iter()
            .map(|record| record.base())
            .collect())
    }
    pub fn view_record(&self, id: &str) -> Result<Option<crate::model::SavedViewRecord>> {
        Ok(self.view_records()?.into_iter().find(|v| v.id == id))
    }
    pub fn annotation_record(&self, id: &str) -> Result<Option<crate::model::AnnotationRecord>> {
        Ok(self.annotation_records()?.into_iter().find(|a| a.id == id))
    }
    pub fn view(&self, id: &str) -> Result<Option<crate::model::SavedView>> {
        Ok(self.view_record(id)?.map(|record| record.base()))
    }
    pub fn annotation(&self, id: &str) -> Result<Option<crate::model::Annotation>> {
        Ok(self.annotation_record(id)?.map(|record| record.base()))
    }
    pub fn put_view_record(&self, record: &crate::model::SavedViewRecord) -> Result<()> {
        record.validate()?;
        self.save(
            RecordItem {
                table: "views",
                id: &record.id,
                node: None,
            },
            serde_json::to_string(record)?,
            false,
        )
        .map(|_| ())
    }
    pub fn update_view_record(
        &self,
        record: &crate::model::SavedViewRecord,
        mut capture: impl FnMut() -> Result<Box<serde_json::value::RawValue>>,
    ) -> Result<crate::model::SavedViewRecord> {
        record.validate()?;
        let mut incoming = record.clone();
        incoming.anchor = None;
        let payload = self.save_with_first_save_hook(
            RecordItem {
                table: "views",
                id: &record.id,
                node: None,
            },
            serde_json::to_string(&incoming)?,
            false,
            |_| capture().map(Some),
            |_| Ok(()),
        )?;
        Ok(serde_json::from_str(&payload)?)
    }
    pub fn put_view(&self, view: &crate::model::SavedView) -> Result<()> {
        view.validate()?;
        self.put_view_record(&crate::model::SavedViewRecord::from_base(
            view.clone(),
            None,
        ))
    }
    pub fn put_annotation_record(
        &self,
        record: &crate::model::AnnotationRecord,
        preserve_title: bool,
    ) -> Result<()> {
        record.validate()?;
        self.save(
            RecordItem {
                table: "annotations",
                id: &record.id,
                node: Some(&record.node_id),
            },
            serde_json::to_string(record)?,
            preserve_title,
        )
        .map(|_| ())
    }
    pub fn update_annotation_record(
        &self,
        record: &crate::model::AnnotationRecord,
        preserve_title: bool,
        mut capture: impl FnMut() -> Result<Box<serde_json::value::RawValue>>,
    ) -> Result<crate::model::AnnotationRecord> {
        record.validate()?;
        let mut incoming = record.clone();
        incoming.anchor = None;
        let payload = self.save_with_first_save_hook(
            RecordItem {
                table: "annotations",
                id: &record.id,
                node: Some(&record.node_id),
            },
            serde_json::to_string(&incoming)?,
            preserve_title,
            |_| capture().map(Some),
            |_| Ok(()),
        )?;
        Ok(serde_json::from_str(&payload)?)
    }
    pub fn put_annotation(&self, annotation: &crate::model::Annotation) -> Result<()> {
        self.put_annotation_with_first_save_hook(annotation, |_| Ok(()))
    }
    /// Fixture fault at the first record's commit or post-commit sync boundary.
    pub fn put_annotation_with_first_save_hook(
        &self,
        annotation: &crate::model::Annotation,
        hook: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        annotation.validate()?;
        let record = crate::model::AnnotationRecord::from_base(annotation.clone(), None, None);
        self.save_with_first_save_hook(
            RecordItem {
                table: "annotations",
                id: &annotation.id,
                node: Some(&annotation.node_id),
            },
            serde_json::to_string(&record)?,
            true,
            |_| Ok(None),
            hook,
        )
        .map(|_| ())
    }
    fn delete(&self, table: &str, id: &str) -> Result<bool> {
        if !self.existing()? {
            return Ok(false);
        }
        let guard = self.lock_existing_writer()?;
        let mut db = self.db(true)?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = if table == "views" {
            tx.execute("DELETE FROM views WHERE id=?1", [id])?
        } else {
            tx.execute("DELETE FROM annotations WHERE id=?1", [id])?
        };
        self.identity.verify()?;
        guard.verify()?;
        tx.commit()?;
        drop(db);
        Ok(changed != 0)
    }
    pub fn delete_view(&self, id: &str) -> Result<bool> {
        self.delete("views", id)
    }
    pub fn delete_annotation(&self, id: &str) -> Result<bool> {
        self.delete("annotations", id)
    }
}

/// The report is a read-only snapshot; it never creates a use lock or a database.
/// `eligible` never deletes a derived index. Future #16 automatic GC needs a
/// separate guarded public contract: verified exclusive use lock, exact shape,
/// typed root/age, hot-journal/live refusal, and retained-pin/record safety.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcReport {
    pub derived: Vec<DerivedReport>,
    pub records: Vec<GcRecordReport>,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedReport {
    pub root_key: String,
    pub status: &'static str,
    pub reason: &'static str,
}
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordReport {
    pub id: String,
    pub views: i64,
    pub annotations: i64,
    pub missing_known_paths: Vec<String>,
}
/// GC can report a record without reading its inventory; forget cannot.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GcRecordReport {
    pub id: String,
    pub status: &'static str,
    pub reason: &'static str,
    // Do not present missing or untrusted inventory as zero saved items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_known_paths: Option<Vec<String>>,
}
impl GcRecordReport {
    fn unavailable(id: String, status: &'static str, reason: &'static str) -> Self {
        Self {
            id,
            status,
            reason,
            views: None,
            annotations: None,
            missing_known_paths: None,
        }
    }
}
impl From<RecordReport> for GcRecordReport {
    fn from(report: RecordReport) -> Self {
        Self {
            id: report.id,
            status: "valid",
            reason: "record_readable",
            views: Some(report.views),
            annotations: Some(report.annotations),
            missing_known_paths: Some(report.missing_known_paths),
        }
    }
}

const GC_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_TRUSTED_SECONDS: i64 = 9_007_199_254_740_991;

/// A persisted open age is trusted only when positive and no later than this report's clock.
pub fn classify_index_age(now_secs: i64, opened_secs: Option<i64>) -> (&'static str, &'static str) {
    match opened_secs {
        Some(opened) if opened > 0 && opened <= MAX_TRUSTED_SECONDS && opened <= now_secs => {
            if now_secs - opened >= GC_AGE_SECONDS {
                ("eligible", "age_30_days")
            } else {
                ("unknown", "recent_open")
            }
        }
        _ => ("unknown", "age_unknown"),
    }
}

fn managed_existing(base: &Path, child: &str) -> Result<Option<PathBuf>> {
    for path in [base.to_owned(), base.join(child)] {
        match fs::symlink_metadata(&path) {
            Ok(_) => private_dir(&path)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(base.join(child)))
}
fn lower_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
/// Accept only an exact durable record ID, never a pathname or a nil UUID.
pub fn valid_record_id(id: &str) -> bool {
    id.strip_prefix("path-")
        .is_some_and(|key| lower_hex(key, 64))
        || Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id)
}
#[derive(Debug)]
enum RecordIssue {
    RecoverySidecar,
    UnsafeDirectory,
    Incompatible(&'static str),
    Incomplete(&'static str),
}
impl std::fmt::Display for RecordIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecoverySidecar => f.write_str("recovery sidecar present"),
            Self::UnsafeDirectory => f.write_str("unsafe record directory"),
            Self::Incompatible(detail) => write!(f, "incompatible_record: {detail}"),
            Self::Incomplete(detail) => write!(f, "incomplete_record: {detail}"),
        }
    }
}
impl std::error::Error for RecordIssue {}

fn readonly_index_db(path: &Path) -> Result<super::ProtectedSqliteConnection> {
    use rusqlite::OpenFlags;
    let file = super::retained_sqlite_file(path, false, false, false)?;
    private_file(path, &file)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path.with_file_name(format!(
            "{}{}",
            path.file_name().unwrap().to_string_lossy(),
            suffix
        ));
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => return Err(RecordIssue::RecoverySidecar.into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let db = super::protected_sqlite_open(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    private_file(path, &file)?;
    db.busy_timeout(Duration::ZERO)?;
    db.pragma_update(None, "query_only", "ON")?;
    let journal: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    ensure!(journal == "delete", "incompatible database journal mode");
    Ok(db)
}
fn readonly_db(path: &Path) -> Result<rusqlite::Connection> {
    use rusqlite::{Connection, OpenFlags};
    let file = open_file_readonly(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = path.with_file_name(format!(
            "{}{suffix}",
            path.file_name().unwrap().to_string_lossy()
        ));
        match fs::symlink_metadata(&sidecar) {
            Ok(_) => return Err(RecordIssue::RecoverySidecar.into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    private_file(path, &file)?;
    db.busy_timeout(Duration::ZERO)?;
    db.pragma_update(None, "query_only", "ON")?;
    let journal: String = db.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
    ensure!(journal == "delete", "incompatible database journal mode");
    Ok(db)
}
fn inspect_index(dir: &Path, key: &str, now_secs: i64) -> Result<(&'static str, &'static str)> {
    inspect_index_with_open_hook(dir, key, now_secs, |_| Ok(()))
}
fn inspect_index_with_open_hook(
    dir: &Path,
    key: &str,
    now_secs: i64,
    before_snapshot: impl FnOnce(&rusqlite::Connection) -> Result<()>,
) -> Result<(&'static str, &'static str)> {
    private_dir(dir)?;
    let mut connection = readonly_index_db(&dir.join("index.db"))?;
    before_snapshot(&connection)?;
    let tx = connection.transaction()?;
    let db = &tx;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Any earlier schema is disposable and never readable by this binary
    // (#253): collect it without inspecting its layout, root or age.
    if old_index_schema(version) {
        return Ok(("eligible", "old_schema"));
    }
    if version != i64::from(super::DATABASE_SCHEMA_VERSION) {
        return Ok(("unknown", "unknown_index_schema"));
    }
    if super::validate_cache_shape(db).is_err() {
        return Ok(("unknown", "unknown_index_shape"));
    }
    if super::validate_supersessions(db).is_err() {
        return Ok(("unknown", "invalid_supersession_inventory"));
    }
    let count: i64 = db.query_row("SELECT count(*) FROM index_metadata", [], |r| r.get(0))?;
    ensure!(count == 1, "incompatible index metadata cardinality");
    let (schema, extractor, spelling, dev, ino, age): (i64, String, String, String, String, rusqlite::types::Value) = db.query_row(
        "SELECT schema_version,extractor_version,root_spelling,root_device,root_inode,last_opened_at FROM index_metadata WHERE singleton=1", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
    ensure!(
        schema == version
            && extractor == super::EXTRACTOR_VERSION
            && Path::new(&spelling).is_absolute()
            && hex::encode(Sha256::digest(spelling.as_bytes())) == key,
        "incompatible index identity"
    );
    let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "incompatible index integrity");
    let device: u64 = dev.parse()?;
    let inode: u64 = ino.parse()?;
    ensure!(device > 0 && inode > 0, "invalid root identity");
    let age = match age {
        rusqlite::types::Value::Integer(n) => Some(n),
        _ => None,
    };
    for ancestor in Path::new(&spelling)
        .ancestors()
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        match fs::symlink_metadata(ancestor) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Ok(m) if m.file_type().is_symlink() => return Ok(("unknown", "root_identity_unknown")),
            Ok(_) if *ancestor == Path::new(&spelling) => break,
            Ok(_) => return Ok(("unknown", "root_identity_unknown")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(("eligible", "root_missing"));
            }
            Err(_) => return Ok(("unknown", "root_identity_unknown")),
        }
    }
    match fs::symlink_metadata(&spelling) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(("eligible", "root_missing")),
        Ok(m)
            if m.is_dir()
                && !m.file_type().is_symlink()
                && (m.dev(), m.ino()) == (device, inode) =>
        {
            Ok(classify_index_age(now_secs, age))
        }
        Ok(m) if !m.file_type().is_symlink() => Ok(("eligible", "root_replaced")),
        _ => Ok(("unknown", "root_identity_unknown")),
    }
}

/// Fault stages are exposed only to deterministic storage integration fixtures.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcStage {
    BeforeStampRename,
    BeforeCandidate,
    BeforeCandidateUnlink,
    AfterFirstDbUnlink,
    AfterParentSync,
}

/// Schemas before the current v21 are disposable: no released binary that wrote
/// one can be served by this one, so GC needs no layout, root or age proof.
fn old_index_schema(version: i64) -> bool {
    (1..i64::from(super::DATABASE_SCHEMA_VERSION)).contains(&version)
}

/// Only an earlier schema or the exact current layout can reach the unlink boundary.
/// Validation failures skip the candidate. An unlink/sync failure is returned
/// as an error, including when an earlier derived file was already removed.
fn gc_remove_candidate(
    dir: &Path,
    key: &str,
    now: i64,
    guard: UseGuard,
    current: &WorkspaceIdentity,
    writer: &WriterGuard,
    hook: &mut dyn FnMut(GcStage) -> Result<()>,
) -> Result<bool> {
    let admit = (|| -> Result<Vec<(PathBuf, (u64, u64))>> {
        guard.verify_exclusive_path(&dir.with_extension("lock"))?;
        private_dir(dir)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF8 GC entry"))?;
            ensure!(
                matches!(name.as_str(), "index.db" | "requests.db"),
                "unknown derived index entry"
            );
            names.push(name);
        }
        ensure!(names.iter().any(|n| n == "index.db"), "incomplete GC index");
        let mut index = readonly_index_db(&dir.join("index.db"))?;
        let tx = index.transaction()?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !old_index_schema(version) {
            ensure!(
                version == i64::from(super::DATABASE_SCHEMA_VERSION),
                "unknown GC schema"
            );
            super::validate_cache_shape(&tx)?;
            super::validate_supersessions(&tx)?;
            let (generation, revision, stats, diagnostics): (String, i64, String, String) = tx.query_row(
            "SELECT index_generation,index_revision,stats,diagnostics FROM index_metadata WHERE singleton=1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
            let uuid = Uuid::parse_str(&generation)?;
            ensure!(
                !uuid.is_nil() && uuid.to_string() == generation && revision >= 0,
                "GC metadata generation or revision unknown"
            );
            serde_json::from_str::<serde_json::Value>(&stats)?;
            serde_json::from_str::<serde_json::Value>(&diagnostics)?;
            let age: rusqlite::types::Value = tx.query_row(
                "SELECT last_opened_at FROM index_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            ensure!(
                matches!(age, rusqlite::types::Value::Integer(value) if value > 0 && value <= now),
                "GC timestamp unknown or in future"
            );
            drop(tx);
            drop(index);
            // This inspection is fresh under EX.
            ensure!(
                inspect_index(dir, key, now)?.0 == "eligible",
                "GC index is live or unknown"
            );
            if names.iter().any(|n| n == "requests.db") {
                let mut queue = readonly_index_db(&dir.join("requests.db"))?;
                let tx = queue.transaction()?;
                let index = readonly_index_db(&dir.join("index.db"))?;
                let spelling: String = index.query_row(
                    "SELECT root_spelling FROM index_metadata WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )?;
                super::requests::validate_gc_queue(&tx, &spelling, key)?;
                drop(index);
                drop(tx);
                drop(queue);
            }
        } else {
            // Close SQLite before the witness loop opens its own handles: closing
            // any other descriptor on a live SQLite file drops its POSIX locks.
            drop(tx);
            drop(index);
            if names.iter().any(|n| n == "requests.db") {
                // Opening registers the queue's witness for the unlink checks.
                drop(readonly_index_db(&dir.join("requests.db"))?);
            }
        }
        let mut dbs = Vec::new();
        for name in ["index.db", "requests.db"] {
            if !names.iter().any(|n| n == name) {
                continue;
            }
            let path = dir.join(name);
            let file = open_file_readonly(&path)?;
            let meta = file.metadata()?;
            dbs.push((path, (meta.dev(), meta.ino())));
        }
        guard.verify()?;
        Ok(dbs)
    })();
    let Ok(dbs) = admit else { return Ok(false) };
    // Recheck the *entire* directory inventory after SQLite closes, before the
    // first unlink. A new hot journal or unknown entry is always a skip.
    let actual = (|| -> Result<Vec<String>> {
        private_dir(dir)?;
        let mut names = fs::read_dir(dir)?
            .map(|entry| {
                entry?
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("non-UTF8 GC entry"))
            })
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    })();
    let Ok(actual) = actual else { return Ok(false) };
    let mut expected = vec!["index.db".to_owned()];
    if dbs.len() == 2 {
        expected.push("requests.db".to_owned());
    }
    expected.sort();
    if actual != expected {
        return Ok(false);
    }
    guard.verify()?;
    hook(GcStage::BeforeCandidateUnlink)?;
    current.verify()?;
    writer.verify()?;
    guard.verify()?;
    if !super::gc_unlink_sqlite(&dbs, &guard, current, writer, &mut || {
        hook(GcStage::AfterFirstDbUnlink)
    })? {
        return Ok(false);
    }
    // Past this boundary every I/O or authority failure is reported; never
    // claim a skip after even one derived inode has been removed.
    current.verify()?;
    writer.verify()?;
    current.verify()?;
    writer.verify()?;
    fs::remove_dir(dir)?;
    sync_directory(dir.parent().context("GC parent missing")?)?;
    hook(GcStage::AfterParentSync)?;
    current.verify()?;
    writer.verify()?;
    // Lock path is the final name removed, after the directory is gone.
    guard.remove_last()?;
    Ok(true)
}

fn ensure_safe_record_contents(dir: &Path) -> Result<()> {
    private_dir(dir)?;
    let mut count = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        ensure!(
            entry.file_name() == "workspace.db",
            "unsafe record contents: unknown entry"
        );
        let file = open_file(&entry.path(), false)?;
        private_file(&entry.path(), &file)?;
        count += 1;
    }
    ensure!(count == 1, "incomplete_record: missing database");
    Ok(())
}
fn validate_record_schema(db: &rusqlite::Connection) -> Result<()> {
    type SchemaObject = (String, String, String, Option<String>);
    fn objects(db: &rusqlite::Connection) -> rusqlite::Result<Vec<SchemaObject>> {
        db.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect()
    }
    // SQLite generates internal autoindex names for the package's PRIMARY KEY constraints.
    // Compare those as well as the original table SQL, not just queryable columns: a
    // countable record with weaker constraints must never be destroyed automatically.
    let expected = rusqlite::Connection::open_in_memory()?;
    expected.execute_batch(RECORD_SCHEMA)?;
    if objects(db)? != objects(&expected)? {
        return Err(RecordIssue::Incompatible("unexpected SQLite schema").into());
    }
    Ok(())
}

fn inspect_record(dir: &Path, id: &str) -> Result<(RecordReport, Vec<String>)> {
    private_dir(dir)?;
    let db = readonly_db(&dir.join("workspace.db")).map_err(|error| {
        // This stage runs only after private_dir(dir) succeeds. A missing DB is
        // incomplete; an unsafe directory must never trigger a child path probe.
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        {
            anyhow::Error::new(RecordIssue::Incomplete("missing database"))
        } else {
            error
        }
    })?;
    let version: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 1 {
        return Err(RecordIssue::Incompatible("schema version").into());
    }
    validate_record_schema(&db)?;
    let row: (i64, String, i64) = db.query_row(
        "SELECT schema_version,record_id,initialized FROM record_metadata WHERE singleton=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if row != (1, id.to_owned(), 1) {
        return Err(RecordIssue::Incomplete("metadata mismatch").into());
    }
    let integrity: String = db.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(RecordIssue::Incomplete("database integrity check failed").into());
    }
    db.prepare("SELECT path,device,inode FROM known_roots")?;
    db.prepare("SELECT id,payload FROM views")?;
    db.prepare("SELECT id,node_id,payload FROM annotations")?;
    let views = db.query_row("SELECT count(*) FROM views", [], |r| r.get(0))?;
    let annotations = db.query_row("SELECT count(*) FROM annotations", [], |r| r.get(0))?;
    let paths = db
        .prepare("SELECT path FROM known_roots ORDER BY path")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let missing_known_paths = paths
        .iter()
        .filter(|p| {
            fs::symlink_metadata(p).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        })
        .cloned()
        .collect();
    Ok((
        RecordReport {
            id: id.to_owned(),
            views,
            annotations,
            missing_known_paths,
        },
        paths,
    ))
}

impl TopologyRoots {
    /// Resolve a durable record by validated ID without discovering a checkout marker.
    pub fn record_by_id(&self, id: &str) -> Result<Option<RecordReport>> {
        let Some((parent, dir)) = self.existing_record_paths(id)? else {
            return Ok(None);
        };
        let guard = UseGuard::acquire_existing(&parent.join(format!("{id}.lock")), false, true)?;
        let (report, _) = inspect_record(&dir, id)?;
        guard.verify()?;
        Ok(Some(report))
    }
    fn existing_record_paths(&self, id: &str) -> Result<Option<(PathBuf, PathBuf)>> {
        ensure!(valid_record_id(id), "invalid record ID");
        let Some(parent) = managed_existing(&self.data, "workspaces")? else {
            return Ok(None);
        };
        let dir = parent.join(id);
        match fs::symlink_metadata(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
            Ok(_) => {
                private_dir(&dir)?;
                Ok(Some((parent, dir)))
            }
        }
    }
    /// Hold the existing exclusive use lock across inventory, confirmation and removal.
    pub fn forget_with_confirmation(
        &self,
        id: &str,
        confirm: impl FnOnce(&RecordReport, &[String]) -> Result<bool>,
    ) -> Result<bool> {
        let (parent, dir) = self
            .existing_record_paths(id)?
            .context("record not found")?;
        let guard = UseGuard::acquire_existing(&parent.join(format!("{id}.lock")), true, true)
            .context("incomplete_record: missing, unsafe or busy use lock")?;
        let (report, paths) = inspect_record(&dir, id)?;
        ensure_safe_record_contents(&dir)?;
        guard.verify()?;
        if !confirm(&report, &paths)? {
            return Ok(false);
        }
        private_dir(&dir)?;
        guard.verify()?;
        ensure_safe_record_contents(&dir)?;
        // Reinspect the database after user input, before removing any bytes.
        let _ = inspect_record(&dir, id)?;
        let db = dir.join("workspace.db");
        let file = open_file(&db, false)?;
        private_file(&db, &file)?;
        drop(file);
        fs::remove_file(&db)?;
        sync_directory(&dir)?;
        fs::remove_dir(&dir)?;
        sync_directory(&parent)?;
        guard.remove_last()?;
        Ok(true)
    }
    /// The advisory report is never a deletion permit. A verified writer records
    /// its bounded daily attempt before opening any candidate. Every candidate
    /// gets its own nonblocking EX and an independent current-schema inspection.
    pub fn automatic_gc_at(
        &self,
        current: &WorkspaceIdentity,
        writer: &WriterGuard,
        now_secs: i64,
    ) -> Result<usize> {
        self.automatic_gc_at_with_hook(current, writer, now_secs, &mut |_| Ok(()))
    }

    #[doc(hidden)]
    pub fn automatic_gc_at_with_hook(
        &self,
        current: &WorkspaceIdentity,
        writer: &WriterGuard,
        now_secs: i64,
        hook: &mut dyn FnMut(GcStage) -> Result<()>,
    ) -> Result<usize> {
        current.verify()?;
        writer.verify()?;
        ensure!(
            writer.use_guard.path == self.index_use_lock(current),
            "unsafe_index: foreign GC writer"
        );
        writer.belongs_to(current)?;
        ensure!(
            now_secs > 0 && now_secs <= MAX_TRUSTED_SECONDS,
            "invalid GC clock"
        );
        private_dir(&self.cache)?;
        let schedule = UseGuard::acquire(&self.cache.join("gc-schedule.lock"), true, true)?;
        let stamp = self.cache.join("gc-last-run");
        let prior = match fs::symlink_metadata(&stamp) {
            Ok(_) => Some(open_file_readonly(&stamp)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(prior) = &prior {
            let mut bytes = Vec::new();
            prior.take(32).read_to_end(&mut bytes)?;
            ensure!(!bytes.is_empty(), "invalid GC stamp: empty existing marker");
            let text = std::str::from_utf8(&bytes)?;
            let last: i64 = text
                .strip_suffix('\n')
                .context("invalid GC stamp")?
                .parse()?;
            ensure!(
                last > 0 && last <= now_secs,
                "invalid GC stamp or clock rollback"
            );
            if now_secs - last < 24 * 60 * 60 {
                return Ok(0);
            }
        }
        // Never truncate the old attempt. A crash before rename leaves its
        // original inode and 24-hour fence intact; a crash after rename sees
        // either the old or the fsynced new stamp.
        let stage = self
            .cache
            .join(format!("gc-last-run.tmp-{}", Uuid::new_v4()));
        let stage_result = (|| -> Result<()> {
            let mut staged = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&stage)?;
            private_file(&stage, &staged)?;
            staged.write_all(format!("{now_secs}\n").as_bytes())?;
            staged.sync_all()?;
            schedule.verify()?;
            current.verify()?;
            writer.verify()?;
            hook(GcStage::BeforeStampRename)?;
            match &prior {
                Some(prior) => private_file(&stamp, prior)?,
                None => ensure!(
                    fs::symlink_metadata(&stamp)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                    "GC stamp appeared after admission"
                ),
            }
            fs::rename(&stage, &stamp)?;
            sync_directory(&self.cache)?;
            Ok(())
        })();
        if stage_result.is_err()
            && let Ok(stage_file) = open_file_readonly(&stage)
        {
            drop(stage_file);
            let _ = fs::remove_file(&stage);
            let _ = sync_directory(&self.cache);
        }
        stage_result?;
        schedule.verify()?;
        let Some(parent) = managed_existing(&self.cache, "indexes")? else {
            return Ok(0);
        };
        let mut removed = 0;
        for entry in fs::read_dir(&parent)? {
            writer.verify()?;
            current.verify()?;
            let entry = entry?;
            let Ok(key) = entry.file_name().into_string() else {
                continue;
            };
            if !lower_hex(&key, 64) || key == current.root_key {
                continue;
            }
            hook(GcStage::BeforeCandidate)?;
            let path = entry.path();
            let guard =
                match UseGuard::acquire_existing(&parent.join(format!("{key}.lock")), true, true) {
                    Ok(guard) => guard,
                    Err(_) => continue,
                };
            // Refuse unknown and unsafe entries without creating or recovering files.
            if gc_remove_candidate(&path, &key, now_secs, guard, current, writer, hook)? {
                removed += 1;
            }
        }
        Ok(removed)
    }
    pub fn gc_report(&self) -> Result<GcReport> {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs() as i64;
        self.gc_report_at(now_secs)
    }
    /// Fixed clock for report tests; production takes one snapshot per invocation.
    pub fn gc_report_at(&self, now_secs: i64) -> Result<GcReport> {
        let mut derived = Vec::new();
        if let Some(parent) = managed_existing(&self.cache, "indexes")? {
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                // Index keys are ASCII hex; no non-UTF8 name can be a managed key.
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !lower_hex(&name, 64) {
                    continue;
                }
                let lock = parent.join(format!("{name}.lock"));
                let (status, reason) = match UseGuard::acquire_existing_readonly_exclusive(&lock) {
                    Err(e) if e.is::<StorageBusy>() => ("busy", "use_lock_busy"),
                    Err(_) => ("unknown", "unsafe_use_lock"),
                    Ok(guard) => {
                        let result = inspect_index(&entry.path(), &name, now_secs)
                            .unwrap_or(("unknown", "metadata_unreadable"));
                        if guard.verify().is_err() {
                            ("unknown", "unsafe_use_lock")
                        } else {
                            result
                        }
                    }
                };
                derived.push(DerivedReport {
                    root_key: name,
                    status,
                    reason,
                });
            }
            derived.sort_by(|a, b| a.root_key.cmp(&b.root_key));
        }
        let mut records = Vec::new();
        if let Some(parent) = managed_existing(&self.data, "workspaces")? {
            for entry in fs::read_dir(&parent)? {
                let entry = entry?;
                // Durable IDs are ASCII; unrelated non-UTF8 entries are not records.
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !valid_record_id(&name) {
                    continue;
                }
                // Report one safely named record at a time. Never create a missing use
                // lock or expose error text containing local paths or SQLite details.
                let lock = parent.join(format!("{name}.lock"));
                let report = match UseGuard::acquire_existing_readonly_exclusive(&lock) {
                    Err(e) if e.is::<StorageBusy>() => {
                        GcRecordReport::unavailable(name, "busy", "use_lock_busy")
                    }
                    Err(_) => GcRecordReport::unavailable(name, "unknown", "unsafe_use_lock"),
                    Ok(guard) => {
                        let result = if private_dir(&entry.path()).is_ok() {
                            inspect_record(&entry.path(), &name)
                        } else {
                            Err(RecordIssue::UnsafeDirectory.into())
                        };
                        if guard.verify().is_err() {
                            GcRecordReport::unavailable(name, "unknown", "unsafe_use_lock")
                        } else {
                            match result {
                                Ok((report, _)) => report.into(),
                                Err(e) => {
                                    let reason = match e.downcast_ref::<RecordIssue>() {
                                        Some(RecordIssue::RecoverySidecar) => "recovery_sidecar",
                                        Some(RecordIssue::UnsafeDirectory) => {
                                            "unsafe_record_directory"
                                        }
                                        Some(RecordIssue::Incompatible(_)) => "incompatible_record",
                                        Some(RecordIssue::Incomplete(_)) => "incomplete_record",
                                        None => "metadata_unreadable",
                                    };
                                    GcRecordReport::unavailable(name, "unknown", reason)
                                }
                            }
                        }
                    }
                };
                records.push(report);
            }
            records.sort_by(|a, b| a.id.cmp(&b.id));
        }
        Ok(GcReport { derived, records })
    }
}

#[cfg(test)]
pub fn assert_topology_fixture(store: &crate::store::Store, state: &Path) {
    let baseline = store.index_baseline().unwrap();
    assert_eq!(baseline.index_revision, 0);
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    let identity = WorkspaceIdentity::discover(
        Some(Path::new(&store.workspace_root)),
        Path::new(&store.workspace_root),
    )
    .unwrap();
    let roots = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
    let index = roots.index_dir(&identity);
    assert_eq!(index.parent().unwrap(), state.join("cache/indexes"));
    for path in [&roots.cache, &roots.cache.join("indexes"), &index] {
        private_dir(path).unwrap();
    }
    for path in [roots.index_db(&identity), roots.index_use_lock(&identity)] {
        let file = open_file(&path, false).unwrap();
        private_file(&path, &file).unwrap();
    }
    assert!(
        !roots.record_db(&identity).exists(),
        "fixture open must not eagerly create a durable record"
    );
    let shared =
        UseGuard::acquire_existing(&roots.index_use_lock(&identity), false, false).unwrap();
    assert!(UseGuard::acquire_existing(&roots.index_use_lock(&identity), true, true).is_err());
    drop(shared);
}

#[cfg(test)]
mod gc_schema_race_tests {
    use super::*;
    use std::{
        cell::RefCell,
        fs,
        os::unix::fs::PermissionsExt,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn extra_view_after_readonly_open_before_snapshot_never_attests_gc_or_deletion() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let store = crate::store::Store::open_for_tests(state.path(), work.path()).unwrap();
        let pin = store.index_baseline().unwrap();
        let path = roots.index_db(&identity);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let after_external = RefCell::new(None);
        let refused = inspect_index_with_open_hook(
            &roots.index_dir(&identity),
            &identity.root_key,
            now,
            |_checked_readonly| {
                // The readonly connection has already checked journal/permissions;
                // this second SQLite connection changes the schema before its snapshot.
                let attacker = rusqlite::Connection::open(&path)?;
                attacker.execute_batch("CREATE VIEW gc_after_admission AS SELECT 1")?;
                drop(attacker);
                *after_external.borrow_mut() = Some(fs::read(&path)?);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(refused, ("unknown", "unknown_index_shape"));
        let ddl_bytes = after_external.into_inner().unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            ddl_bytes,
            "read-only GC must not write after external VIEW creation"
        );
        let derived = roots.gc_report_at(now).unwrap().derived;
        assert_eq!(derived.len(), 1);
        assert_eq!(
            (derived[0].status, derived[0].reason),
            ("unknown", "unknown_index_shape")
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            ddl_bytes,
            "GC report must not change attacker-created VIEW or index bytes"
        );
        let attacker = rusqlite::Connection::open(&path).unwrap();
        let (schema, marker, generation, revision): (i64,String,String,i64) = attacker.query_row(
            "SELECT schema_version,extractor_version,index_generation,index_revision FROM index_metadata", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(
            (schema, marker.as_str(), generation, revision),
            (
                21,
                "native-v6-delta-v1",
                pin.index_generation.to_string(),
                pin.index_revision as i64
            )
        );
        assert!(
            store
                .status()
                .unwrap_err()
                .to_string()
                .contains("incompatible_index")
        );
    }

    /// #253: an earlier schema is deleted whatever its layout, root or age,
    /// while the in-use and directory-inventory fences still apply.
    #[test]
    fn old_schema_index_is_collected_regardless_of_layout_root_or_age() {
        let now = 1_800_000_000_i64;
        for version in [1_i64, 8, 9, 12, 13] {
            let state = tempfile::tempdir().unwrap();
            let work = tempfile::tempdir().unwrap();
            let roots = TopologyRoots::isolated_for_tests(
                state.path().join("cache"),
                state.path().join("data"),
            );
            let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
            drop(crate::store::Store::open_for_tests(state.path(), work.path()).unwrap());
            let dir = roots.index_dir(&identity);
            let path = roots.index_db(&identity);
            let stale = fs::symlink_metadata(&path).unwrap();
            let ex =
                UseGuard::acquire_existing(&roots.index_use_lock(&identity), true, true).unwrap();
            fs::remove_file(&path).unwrap();
            crate::store::release_deleted_sqlite_witness(&path, (stale.dev(), stale.ino()), &ex)
                .unwrap();
            drop(ex);
            // A live root and no metadata at all: only the version counts.
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch("CREATE TABLE anything(x)").unwrap();
            db.pragma_update(None, "user_version", version).unwrap();
            drop(db);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(
                inspect_index(&dir, &identity.root_key, now).unwrap(),
                ("eligible", "old_schema"),
                "v{version}"
            );
            let other = tempfile::tempdir().unwrap();
            drop(crate::store::Store::open_for_tests(state.path(), other.path()).unwrap());
            let current = WorkspaceIdentity::discover(Some(other.path()), other.path()).unwrap();
            let writer = roots
                .writer(Arc::new(current.verified_clone().unwrap()))
                .unwrap();
            let remove = || {
                let guard =
                    UseGuard::acquire_existing(&roots.index_use_lock(&identity), true, true)
                        .unwrap();
                gc_remove_candidate(
                    &dir,
                    &identity.root_key,
                    now,
                    guard,
                    &current,
                    &writer,
                    &mut |_| Ok(()),
                )
                .unwrap()
            };
            if version == 12 {
                let before = fs::read(&path).unwrap();
                // A borrowed SQLite inode blocks deletion.
                let borrowed =
                    crate::store::retained_sqlite_file(&path, false, false, false).unwrap();
                assert!(!remove(), "borrowed witness must block v{version} deletion");
                drop(borrowed);
                // So does a hot journal: the directory inventory is not exact.
                let journal = path.with_file_name("index.db-journal");
                fs::write(&journal, b"hot-journal-sentinel").unwrap();
                assert!(!remove(), "journal must block v{version} deletion");
                assert_eq!(fs::read(&journal).unwrap(), b"hot-journal-sentinel");
                fs::remove_file(journal).unwrap();
                assert_eq!(fs::read(&path).unwrap(), before);
            }
            assert!(remove(), "v{version}");
            assert!(!path.exists() && !dir.exists(), "v{version}");
        }
    }

    #[test]
    fn old_schema_report_is_eligible_and_writes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let store = crate::store::Store::open_for_tests(state.path(), work.path()).unwrap();
        drop(store);
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let path = roots.index_db(&identity);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.pragma_update(None, "user_version", 7_i64).unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let report = roots.gc_report_at(1_800_000_000).unwrap();
        assert_eq!(report.derived.len(), 1);
        assert_eq!(
            (report.derived[0].status, report.derived[0].reason),
            ("eligible", "old_schema")
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn exact_v17_gc_candidate_survives_unknown_busy_then_deletes_after_fences_clear() {
        let state = tempfile::tempdir().unwrap();
        let candidate_root = tempfile::tempdir().unwrap();
        let current_root = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let candidate =
            WorkspaceIdentity::discover(Some(candidate_root.path()), candidate_root.path())
                .unwrap();
        let current =
            WorkspaceIdentity::discover(Some(current_root.path()), current_root.path()).unwrap();
        drop(crate::store::Store::open_for_tests(state.path(), candidate_root.path()).unwrap());
        drop(crate::store::Store::open_for_tests(state.path(), current_root.path()).unwrap());
        let now = 1_800_000_000_i64;
        let path = roots.index_db(&candidate);
        let dir = roots.index_dir(&candidate);
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 86_400],
        )
        .unwrap();
        drop(db);
        let writer = roots
            .writer(Arc::new(current.verified_clone().unwrap()))
            .unwrap();
        assert_eq!(
            inspect_index(&dir, &candidate.root_key, now).unwrap(),
            ("eligible", "age_30_days")
        );
        let busy_guard =
            UseGuard::acquire_existing(&roots.index_use_lock(&candidate), true, true).unwrap();
        assert_eq!(
            roots
                .gc_report_at(now)
                .unwrap()
                .derived
                .into_iter()
                .find(|entry| entry.root_key == candidate.root_key)
                .unwrap()
                .status,
            "busy"
        );
        drop(busy_guard);
        let journal = path.with_file_name("index.db-journal");
        fs::write(&journal, b"hot-journal-sentinel").unwrap();
        let guard =
            UseGuard::acquire_existing(&roots.index_use_lock(&candidate), true, true).unwrap();
        assert!(
            !gc_remove_candidate(
                &dir,
                &candidate.root_key,
                now,
                guard,
                &current,
                &writer,
                &mut |_| Ok(())
            )
            .unwrap()
        );
        assert!(path.exists());
        fs::remove_file(journal).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE VIEW unknown_gc_shape AS SELECT 1")
            .unwrap();
        drop(db);
        let guard =
            UseGuard::acquire_existing(&roots.index_use_lock(&candidate), true, true).unwrap();
        assert!(
            !gc_remove_candidate(
                &dir,
                &candidate.root_key,
                now,
                guard,
                &current,
                &writer,
                &mut |_| Ok(())
            )
            .unwrap()
        );
        assert_eq!(
            inspect_index(&dir, &candidate.root_key, now).unwrap(),
            ("unknown", "unknown_index_shape")
        );
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("DROP VIEW unknown_gc_shape").unwrap();
        drop(db);
        assert_eq!(roots.automatic_gc_at(&current, &writer, now).unwrap(), 1);
        assert!(!path.exists());
        assert!(!dir.exists());
    }

    #[test]
    fn stale_v8_extractor_marker_is_refused_without_gc_writes_or_deletion() {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let identity = WorkspaceIdentity::discover(Some(work.path()), work.path()).unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let store = crate::store::Store::open_for_tests(state.path(), work.path()).unwrap();
        let pin = store.index_baseline().unwrap();
        let dir = roots.index_dir(&identity);
        let path = roots.index_db(&identity);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(
            inspect_index(&dir, &identity.root_key, now).unwrap(),
            ("unknown", "recent_open"),
            "genuine current-v17 cache must pass GC admission"
        );

        let attacker = rusqlite::Connection::open(&path).unwrap();
        attacker
            .execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET extractor_version='native-v4' WHERE singleton=1")
            .unwrap();
        assert_eq!(attacker.changes(), 1);
        let (schema, marker, generation, revision): (i64, String, String, i64) = attacker
            .query_row(
                "SELECT schema_version,extractor_version,index_generation,index_revision FROM index_metadata WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (schema, marker.as_str(), generation, revision),
            (
                21,
                "native-v4",
                pin.index_generation.to_string(),
                pin.index_revision as i64
            )
        );
        drop(attacker);

        let footprint = || {
            [
                path.clone(),
                dir.join("index.db-wal"),
                dir.join("index.db-shm"),
                dir.join("index.db-journal"),
            ]
            .map(|p| {
                let bytes = if p.try_exists().unwrap() {
                    Some(fs::read(&p).unwrap())
                } else {
                    None
                };
                (p, bytes)
            })
        };
        let attacked_bytes = footprint();
        assert!(attacked_bytes[0].1.is_some());
        assert!(attacked_bytes[1..].iter().all(|(_, bytes)| bytes.is_none()));
        let refused = inspect_index(&dir, &identity.root_key, now).unwrap_err();
        assert!(
            refused.to_string().contains("incompatible index identity"),
            "{refused:#}"
        );
        assert_eq!(
            footprint(),
            attacked_bytes,
            "inspection must not write DB or sidecars"
        );
        let derived = roots.gc_report_at(now).unwrap().derived;
        assert_eq!(derived.len(), 1);
        assert_eq!(derived[0].root_key, identity.root_key);
        assert_eq!(
            (derived[0].status, derived[0].reason),
            ("unknown", "metadata_unreadable")
        );
        assert!(dir.exists(), "GC report must not delete rejected index");
        assert_eq!(
            footprint(),
            attacked_bytes,
            "GC report must not write DB or sidecars"
        );
    }

    #[test]
    fn root_loss_requires_proven_pathname_change_not_arbitrary_io_failure() {
        let root = tempfile::tempdir().unwrap();
        let identity = WorkspaceIdentity::discover(Some(root.path()), root.path()).unwrap();
        assert!(!identity.root_path_replaced().unwrap());
        let denied = identity.root_path_replaced_from(Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "injected metadata refusal",
        )));
        assert_eq!(denied.unwrap_err().to_string(), "injected metadata refusal");
        assert!(
            identity
                .root_path_replaced_from(Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
                .unwrap()
        );
    }
}

#[cfg(test)]
mod gc_final_witness_tests {
    use super::*;

    #[test]
    fn borrowed_and_live_sqlite_witnesses_skip_at_final_unlink_without_close() {
        let state = tempfile::tempdir().unwrap();
        let current_root = tempfile::tempdir().unwrap();
        let candidate_root = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            state.path().join("cache"),
            state.path().join("data"),
        );
        let current =
            WorkspaceIdentity::discover(Some(current_root.path()), current_root.path()).unwrap();
        let candidate =
            WorkspaceIdentity::discover(Some(candidate_root.path()), candidate_root.path())
                .unwrap();
        drop(crate::store::Store::open_for_tests(state.path(), current_root.path()).unwrap());
        drop(crate::store::Store::open_for_tests(state.path(), candidate_root.path()).unwrap());
        drop(
            roots
                .writer(Arc::new(candidate.verified_clone().unwrap()))
                .unwrap(),
        );
        let writer = roots
            .writer(Arc::new(current.verified_clone().unwrap()))
            .unwrap();
        let now = 1_800_000_000_i64;
        let path = roots.index_db(&candidate);
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE index_metadata SET last_opened_at=?1",
                [now - 31 * 24 * 60 * 60],
            )
            .unwrap();
        let before = fs::read(&path).unwrap();
        let mut borrowed = None;
        let mut hit = 0;
        let result = roots.automatic_gc_at_with_hook(&current, &writer, now, &mut |stage| {
            if stage == GcStage::BeforeCandidateUnlink {
                hit += 1;
                borrowed = Some(crate::store::retained_sqlite_file(
                    &path, false, false, false,
                )?);
            }
            Ok(())
        });
        assert_eq!(hit, 1);
        assert_eq!(
            result.unwrap(),
            0,
            "borrowed witness must skip, not fail or delete"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(borrowed.as_ref().unwrap().metadata().unwrap().nlink() == 1);
        drop(borrowed);

        let mut live = None;
        let mut hit = 0;
        let result =
            roots.automatic_gc_at_with_hook(&current, &writer, now + 86_400, &mut |stage| {
                if stage == GcStage::BeforeCandidateUnlink {
                    hit += 1;
                    live = Some(crate::store::protected_sqlite_open(
                        &path,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                    )?);
                }
                Ok(())
            });
        assert_eq!(hit, 1);
        assert_eq!(
            result.unwrap(),
            0,
            "live SQLite connection must skip, not delete"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let db = live.as_ref().unwrap();
        assert_eq!(
            db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .unwrap(),
            21
        );
        drop(live);
        assert!(roots.index_use_lock(&candidate).exists());
    }
}
