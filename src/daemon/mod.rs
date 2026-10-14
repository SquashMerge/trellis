//! Per-user daemon election and private Unix socket ownership.
pub mod causal_witness;
pub mod client;
pub mod protocol;
pub mod registry;

use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SocketPaths {
    pub run: PathBuf,
    pub lock: PathBuf,
    pub socket: PathBuf,
}

impl SocketPaths {
    pub fn new(data_dir: &Path) -> Self {
        let run = data_dir.join("run");
        let normal = run.join("daemon.sock");
        // sockaddr_un has a platform-fixed path bound (104 bytes on Darwin).
        // Only long private data roots use a separate deterministic private
        // directory. The hash qualifies the full data root and the user ID;
        // the election lock remains in the original per-user run directory.
        #[cfg(target_os = "macos")]
        const MAX_SOCKET_PATH: usize = 103;
        #[cfg(not(target_os = "macos"))]
        const MAX_SOCKET_PATH: usize = 107;
        let socket = if normal.as_os_str().as_encoded_bytes().len() <= MAX_SOCKET_PATH {
            normal
        } else {
            let mut digest = Sha256::new();
            digest.update(data_dir.as_os_str().as_encoded_bytes());
            digest.update(unsafe { libc::geteuid() }.to_be_bytes());
            let digest = digest.finalize();
            let name = format!(
                "trellis-{}-{}",
                unsafe { libc::geteuid() },
                hex::encode(&digest[..16])
            );
            Path::new("/tmp").join(name).join("daemon.sock")
        };
        Self {
            lock: run.join("daemon.lock"),
            socket,
            run,
        }
    }

    fn prepare(&self) -> io::Result<()> {
        // Existing system ancestors are not managed by Trellis. Create each missing
        // component privately, even when the data directory itself is not present.
        let mut missing = Vec::new();
        let mut ancestor = self.run.as_path();
        while !ancestor.exists() {
            missing.push(ancestor.to_path_buf());
            ancestor = ancestor.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "daemon directory has no ancestor",
                )
            })?;
        }
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe daemon ancestor",
            ));
        }
        for dir in missing.iter().rev() {
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
        }
        for dir in [self.run.parent().unwrap(), self.run.as_path()] {
            let metadata = fs::symlink_metadata(dir)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe daemon directory",
                ));
            }
        }
        if self.socket.parent() != Some(self.run.as_path()) {
            let private = self.socket.parent().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "missing private socket directory",
                )
            })?;
            match fs::DirBuilder::new().mode(0o700).create(private) {
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(error),
            }
            let metadata = fs::symlink_metadata(private)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o777 != 0o700
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "unsafe private socket directory",
                ));
            }
        }
        Ok(())
    }
}

pub struct SocketOwner {
    _lock: File,
    listener: UnixListener,
    socket: PathBuf,
    inode: u64,
}

impl SocketOwner {
    /// Only the elected lock holder is allowed to clear a crashed owner's socket.
    pub fn acquire(paths: &SocketPaths) -> io::Result<Option<Self>> {
        paths.prepare()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&paths.lock)?;
        let metadata = lock.metadata()?;
        let named = fs::symlink_metadata(&paths.lock)?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
            || (metadata.dev(), metadata.ino()) != (named.dev(), named.ino())
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe daemon lock",
            ));
        }
        let result = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error)
            };
        }
        match fs::symlink_metadata(&paths.socket) {
            Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(&paths.socket)?,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe daemon socket",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&paths.socket)?;
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        let inode = fs::symlink_metadata(&paths.socket)?.ino();
        Ok(Some(Self {
            _lock: lock,
            listener,
            socket: paths.socket.clone(),
            inode,
        }))
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.socket)
            .is_ok_and(|metadata| metadata.file_type().is_socket() && metadata.ino() == self.inode)
        {
            let _ = fs::remove_file(&self.socket);
        }
    }
}

/// Drive idle release while a daemon owns the registry. The caller retains its
/// socket/listener guard and closes control connections when this returns.
/// Polling also observes work draining after an expired deadline without
/// resetting either clock. The command entry point wires this into its task.
pub async fn run_idle_lifecycle(
    registry: std::sync::Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
) -> Result<(), registry::SelectionError> {
    let mut ticks = tokio::time::interval(std::time::Duration::from_millis(250));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        if registry
            .lock()
            .await
            .advance(std::time::Instant::now())?
            .exit
        {
            return Ok(());
        }
    }
}

/// Observe token data and identity through the same checked descriptor.
fn checked_token(path: &Path) -> anyhow::Result<(String, (u64, u64))> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.mode() & 0o777 == 0o600
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1,
        "insecure token file"
    );
    let mut token = String::new();
    file.by_ref().take(65).read_to_string(&mut token)?;
    anyhow::ensure!(crate::auth::valid_token(&token), "invalid token file");
    Ok((token, (metadata.dev(), metadata.ino())))
}

fn check_token_path(path: &Path, inode: (u64, u64)) -> anyhow::Result<()> {
    let named = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        named.is_file() && !named.file_type().is_symlink() && (named.dev(), named.ino()) == inode,
        "serve token file identity changed"
    );
    Ok(())
}

/// Browser HTTP is absent until explicit serve registration succeeds. The
/// provisioner belongs to the elected daemon, not a checkout attachment.
pub struct BrowserProvisioner {
    listener: Option<tokio::net::TcpListener>,
    browser: Option<crate::http::ProvisionedBrowser>,
    requested_bind: Option<std::net::SocketAddr>,
    token_path: Option<PathBuf>,
    token_inode: Option<(u64, u64)>,
    token: Option<String>,
    /// SERVE_REPLACEMENT_WAIT in production; tests that exercise its expiry
    /// shorten it rather than wait real seconds.
    replacement_wait: std::time::Duration,
    effective_address: Option<std::net::SocketAddr>,
    closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Default for BrowserProvisioner {
    fn default() -> Self {
        Self::new()
    }
}

/// How long serve waits for a CLI-activated runtime to finish its accepted
/// work (H, FIFO, a read) before replacing it. Accepted work is never cut
/// short, and under load an index can take several seconds; this bound only
/// turns a stuck runtime into a typed storage_busy. #138 removes the wait by
/// applying serve's registration in place at the worker's next boundary.
pub const SERVE_REPLACEMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// The advisory watcher-release request a busy CLI-replacement retry arms on
/// one runtime's scheduler. Reset synchronously on every exit from serve
/// registration, including a dropped future. It holds the exact scheduler
/// that was armed and never looks up a successor runtime by root key.
#[derive(Default)]
struct ArmedWatcherRelease(Option<std::sync::Arc<crate::http::DaemonState>>);

impl ArmedWatcherRelease {
    fn retain(&mut self, scheduler: Option<std::sync::Arc<crate::http::DaemonState>>) {
        if let (Some(held), Some(next)) = (&self.0, &scheduler)
            && std::sync::Arc::ptr_eq(held, next)
        {
            return;
        }
        if let Some(previous) = std::mem::replace(&mut self.0, scheduler) {
            previous.reset_watcher_release();
            previous.withdraw_release_request();
        }
    }
}

impl Drop for ArmedWatcherRelease {
    fn drop(&mut self) {
        if let Some(scheduler) = self.0.take() {
            scheduler.reset_watcher_release();
            // A busy retry also requested the release; withdraw it so idle
            // work and maintenance stop yielding to a release nobody awaits.
            scheduler.withdraw_release_request();
        }
    }
}

impl BrowserProvisioner {
    pub fn new() -> Self {
        Self {
            listener: None,
            browser: None,
            requested_bind: None,
            token_path: None,
            token_inode: None,
            token: None,
            replacement_wait: SERVE_REPLACEMENT_WAIT,
            effective_address: None,
            closed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Exercise the replacement wait's typed expiry without real seconds.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_replacement_wait_for_tests(&mut self, wait: std::time::Duration) {
        self.replacement_wait = wait;
    }

    pub fn address(&self) -> Option<std::net::SocketAddr> {
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            None
        } else {
            self.effective_address
        }
    }

    pub fn router(&self) -> Option<axum::Router> {
        self.browser
            .clone()
            .map(crate::http::ProvisionedBrowser::router)
    }

    /// Call under the daemon's provisioning mutex. All conflicts are checked
    /// before touching registration, listener or another checkout's settings.
    pub async fn register_serve(
        &mut self,
        registry: &std::sync::Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
        identity: &crate::store::topology::WorkspaceIdentity,
        options: registry::CheckoutOptions,
        bind: std::net::SocketAddr,
        token_file: &Path,
    ) -> anyhow::Result<std::net::SocketAddr> {
        self.register_serve_with_hook(registry, identity, options, bind, token_file, None)
            .await
    }

    /// Hook for deterministic boundary tests. `retry_wait` (test-support only)
    /// runs after dropping the registry guard; other stages run under it.
    #[doc(hidden)]
    pub async fn register_serve_with_hook(
        &mut self,
        registry: &std::sync::Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
        identity: &crate::store::topology::WorkspaceIdentity,
        options: registry::CheckoutOptions,
        bind: std::net::SocketAddr,
        token_file: &Path,
        mut hook: Option<&mut dyn FnMut(&'static str)>,
    ) -> anyhow::Result<std::net::SocketAddr> {
        anyhow::ensure!(
            bind.ip().is_loopback(),
            "only loopback bind addresses are supported"
        );
        let parent = token_file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file = token_file
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("token file needs a name"))?;
        let token_path = parent.canonicalize()?.join(file);
        // A CLI-only runtime may still be finishing H, a selected read, or
        // accepted work. Wait only for that typed transient state, releasing
        // the registry guard so the runtime can finish. A genuine conflicting
        // registration or attached client remains an immediate refusal.
        let deadline = std::time::Instant::now() + self.replacement_wait;
        // A busy retry may arm the runtime's advisory watcher release. The
        // guard resets it on every exit: success, timeout, a later non-busy
        // error, or this future being dropped.
        let mut armed = ArmedWatcherRelease::default();
        let mut checked = loop {
            let mut checked = registry.lock().await;
            match checked.can_register(identity, &options) {
                Ok(()) => break checked,
                Err(registry::SelectionError::StorageBusy) => {
                    armed.retain(checked.cli_replacement_scheduler(&identity.root_key));
                    drop(checked);
                    if std::time::Instant::now() >= deadline {
                        anyhow::bail!("{}", registry::SelectionError::StorageBusy.reason());
                    }
                    #[cfg(feature = "test-support")]
                    if let Some(hook) = hook.as_mut() {
                        hook("retry_wait");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                Err(error) => anyhow::bail!("{}", error.reason()),
            }
        };
        drop(armed);
        // The same successful guard covers validation, binding, token creation
        // and registration; no other attachment can win between these steps.
        if let Some(hook) = hook.as_mut() {
            hook("validated");
        }
        if let Some(original) = self.requested_bind {
            anyhow::ensure!(self.address().is_some(), "browser listener closed");
            anyhow::ensure!(
                bind == original,
                "serve bind conflicts with running listener"
            );
            anyhow::ensure!(
                self.token_path.as_ref() == Some(&token_path),
                "serve token file conflicts with running listener"
            );
            let (token, inode) = checked_token(&token_path)?;
            if let Some(hook) = hook.as_mut() {
                hook("token_observed");
            }
            check_token_path(&token_path, inode)?;
            anyhow::ensure!(
                self.token_inode == Some(inode),
                "serve token file identity changed"
            );
            anyhow::ensure!(
                self.token.as_ref() == Some(&token),
                "serve token contents changed"
            );
            checked
                .register(identity, options)
                .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
            return self
                .address()
                .ok_or_else(|| anyhow::anyhow!("browser listener closed"));
        }
        let listener = tokio::net::TcpListener::bind(bind).await?;
        let address = listener.local_addr()?;
        crate::auth::load_or_create_token(&token_path)?;
        let (token, inode) = checked_token(&token_path)?;
        if let Some(hook) = hook.as_mut() {
            hook("token_observed");
        }
        check_token_path(&token_path, inode)?;
        let browser =
            crate::http::ProvisionedBrowser::new(registry.clone(), token.clone(), address)?;
        checked
            .register(identity, options)
            .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
        self.listener = Some(listener);
        self.effective_address = Some(address);
        self.browser = Some(browser);
        self.requested_bind = Some(bind);
        self.token_path = Some(token_path);
        self.token_inode = Some(inode);
        self.token = Some(token);
        Ok(address)
    }

    /// Start serving without consuming the provisioner. New explicit serve
    /// registrations can still compare against its retained bind and token.
    /// The caller monitors the task and closes serve control on idle exit.
    pub fn spawn_until_idle(
        &mut self,
        registry: std::sync::Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    ) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| anyhow::anyhow!("browser not provisioned"))?;
        let browser = self
            .browser
            .clone()
            .ok_or_else(|| anyhow::anyhow!("browser not provisioned"))?;
        let closed = self.closed.clone();
        Ok(tokio::spawn(async move {
            let server = axum::serve(listener, browser.router());
            let result = tokio::select! {
                outcome = server => outcome.map_err(anyhow::Error::from),
                outcome = run_idle_lifecycle(registry) => outcome.map_err(|error| anyhow::anyhow!("{}", error.reason())),
            };
            closed.store(true, std::sync::atomic::Ordering::Release);
            result
        }))
    }

    /// The elected socket owner drives the only lifecycle clock. Browser
    /// hosting observes its shutdown signal instead of advancing the same
    /// registry independently.
    pub fn spawn_with_shutdown(
        &mut self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<tokio::task::JoinHandle<anyhow::Result<()>>> {
        let listener = self
            .listener
            .take()
            .ok_or_else(|| anyhow::anyhow!("browser not provisioned"))?;
        let browser = self
            .browser
            .clone()
            .ok_or_else(|| anyhow::anyhow!("browser not provisioned"))?;
        let closed = self.closed.clone();
        Ok(tokio::spawn(async move {
            let result = axum::serve(listener, browser.router())
                .with_graceful_shutdown(async move {
                    while !*shutdown.borrow() && shutdown.changed().await.is_ok() {}
                })
                .await
                .map_err(anyhow::Error::from);
            closed.store(true, std::sync::atomic::Ordering::Release);
            result
        }))
    }

    /// Convenience for a single serve control owner; socket dispatch should
    /// instead keep this provisioner and monitor the spawned task.
    pub async fn serve_until_idle(
        mut self,
        registry: std::sync::Arc<tokio::sync::Mutex<registry::CheckoutRegistry>>,
    ) -> anyhow::Result<()> {
        self.spawn_until_idle(registry)?.await?
    }
}
