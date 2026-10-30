//! Immutable admission of filesystem inputs for one indexing pass.
use crate::{
    indexer::IndexOptions,
    model::{CancelFlag, IndexProgress, SourceFile},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

pub const ROOT_INPUTS: &[&str] = &[
    "package.json",
    "tsconfig.json",
    "jsconfig.json",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "Cargo.toml",
    "Cargo.lock",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
    "pyproject.toml",
    "requirements.txt",
    "uv.lock",
    "poetry.lock",
    "Pipfile",
    "Pipfile.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct Stamp {
    kind: u8,
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    mtime: (i64, i64),
    #[cfg(unix)]
    ctime: (i64, i64),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CaptureStat {
    pub version: u8,
    pub kind: String,
    pub size: u64,
    pub device: Option<u64>,
    pub inode: Option<u64>,
    pub mtime_seconds: Option<i64>,
    pub mtime_nanoseconds: Option<i64>,
    pub ctime_seconds: Option<i64>,
    pub ctime_nanoseconds: Option<i64>,
}

impl Stamp {
    fn persisted(&self) -> CaptureStat {
        CaptureStat {
            version: 1,
            kind: match self.kind {
                1 => "directory",
                2 => "file",
                _ => "other",
            }
            .into(),
            size: self.len,
            #[cfg(unix)]
            device: Some(self.dev),
            #[cfg(not(unix))]
            device: None,
            #[cfg(unix)]
            inode: Some(self.ino),
            #[cfg(not(unix))]
            inode: None,
            #[cfg(unix)]
            mtime_seconds: Some(self.mtime.0),
            #[cfg(not(unix))]
            mtime_seconds: None,
            #[cfg(unix)]
            mtime_nanoseconds: Some(self.mtime.1),
            #[cfg(not(unix))]
            mtime_nanoseconds: None,
            #[cfg(unix)]
            ctime_seconds: Some(self.ctime.0),
            #[cfg(not(unix))]
            ctime_seconds: None,
            #[cfg(unix)]
            ctime_nanoseconds: Some(self.ctime.1),
            #[cfg(not(unix))]
            ctime_nanoseconds: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "state", rename_all = "camelCase")]
pub enum CaptureInputObservation {
    Absent,
    Present {
        stat: CaptureStat,
        hash: String,
    },
    Directory {
        stat: CaptureStat,
    },
    Root {
        stat: CaptureStat,
    },
    /// The executing native extractor's build-time producer hash
    /// (`EXTRACTOR_INPUT_KEY`), not a filesystem observation.
    Extractor {
        hash: String,
    },
}
/// The persisted input key that records the native extractor producer hash.
pub(crate) const EXTRACTOR_INPUT_KEY: &str = "extractor:trellis.native.syntax";
fn stamp(meta: &fs::Metadata) -> Stamp {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Stamp {
        kind: if meta.is_dir() {
            1
        } else if meta.is_file() {
            2
        } else {
            3
        },
        len: meta.len(),
        #[cfg(unix)]
        dev: meta.dev(),
        #[cfg(unix)]
        ino: meta.ino(),
        #[cfg(unix)]
        mtime: (meta.mtime(), meta.mtime_nsec()),
        #[cfg(unix)]
        ctime: (meta.ctime(), meta.ctime_nsec()),
    }
}
fn check(cancel: &CancelFlag) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
    Ok(())
}
fn metadata(path: &Path) -> Result<Option<Stamp>> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(stamp(&m))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("input metadata: {}", path.display())),
    }
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SourceOperations {
    pub opens: usize,
    pub complete_reads: usize,
    pub hashes: usize,
}

#[cfg(test)]
type SourceReadHook = Box<dyn Fn(&Path, &str)>;
#[cfg(test)]
thread_local! {
    static SOURCE_READ_HOOK: std::cell::RefCell<Option<SourceReadHook>> =
        std::cell::RefCell::new(None);
}
#[cfg(test)]
pub(crate) fn with_source_read_hook<T>(
    hook: impl Fn(&Path, &str) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    SOURCE_READ_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            SOURCE_READ_HOOK.with(|slot| *slot.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    run()
}
#[cfg(test)]
fn source_read_hook(path: &Path, phase: &str) {
    SOURCE_READ_HOOK.with(|hook| {
        if let Some(callback) = hook.borrow().as_ref() {
            callback(path, phase);
        }
    });
}

fn open_error(path: &Path, expected: &Stamp, error: std::io::Error) -> Result<Vec<u8>> {
    if metadata(path)?.as_ref() != Some(expected) {
        return Err(SourceInputDrift("input drift during open").into());
    }
    Err(error).with_context(|| format!("opening input: {}", path.display()))
}

fn regular_read(
    path: &Path,
    cap: u64,
    expected: &Stamp,
    cancel: &CancelFlag,
    operations: Option<&mut SourceOperations>,
) -> Result<Vec<u8>> {
    ensure!(
        expected.kind == 2 && expected.len <= cap,
        "unsafe or oversized input: {}",
        path.display()
    );
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(test)]
    source_read_hook(path, "open");
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) => return open_error(path, expected, error),
    };
    let mut operations = operations;
    if let Some(counts) = operations.as_deref_mut() {
        counts.opens += 1;
    }
    if stamp(&file.metadata()?) != *expected {
        return Err(SourceInputDrift("input drift during open").into());
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 65536];
    loop {
        check(cancel)?;
        let n = match file.read(&mut buffer) {
            Ok(n) => n,
            Err(error) => {
                if metadata(path)?.as_ref() != Some(expected)
                    || stamp(&file.metadata()?) != *expected
                {
                    return Err(SourceInputDrift("input drift during read").into());
                }
                return Err(error).with_context(|| format!("reading input: {}", path.display()));
            }
        };
        if n == 0 {
            break;
        }
        #[cfg(test)]
        source_read_hook(path, "read");
        ensure!(
            (bytes.len() as u64) + (n as u64) <= cap,
            "input exceeded byte limit: {}",
            path.display()
        );
        bytes.extend_from_slice(&buffer[..n]);
    }
    if bytes.len() as u64 != expected.len || stamp(&file.metadata()?) != *expected {
        return Err(SourceInputDrift("input drift during read").into());
    }
    if metadata(path)?.as_ref() != Some(expected) {
        return Err(SourceInputDrift("input replaced during read").into());
    }
    if let Some(counts) = operations {
        counts.complete_reads += 1;
    }
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
#[cfg(unix)]
fn identity(item: &Stamp) -> Option<(u64, u64)> {
    Some((item.dev, item.ino))
}
#[cfg(not(unix))]
fn identity(_item: &Stamp) -> Option<(u64, u64)> {
    None
}

fn admit_input(
    path: PathBuf,
    cap: u64,
    cancel: &CancelFlag,
    source_identities: &BTreeSet<(u64, u64)>,
    identities: &mut BTreeMap<(u64, u64), PathBuf>,
    inputs: &mut BTreeMap<PathBuf, Option<Stamp>>,
    input_bytes: &mut BTreeMap<PathBuf, Option<Arc<[u8]>>>,
) -> Result<()> {
    let state = metadata(&path)?;
    let bytes = match &state {
        None => None,
        Some(item) => {
            ensure!(
                item.kind == 2 && item.len <= cap,
                "unsafe or oversized input: {}",
                path.display()
            );
            let key = identity(item);
            ensure!(
                !key.is_some_and(|key| source_identities.contains(&key)),
                "input aliases source: {}",
                path.display()
            );
            if let Some(old_path) = key.and_then(|key| identities.get(&key)) {
                #[cfg(test)]
                source_read_hook(old_path, "alias");
                if metadata(old_path)? != inputs[old_path] {
                    return Err(SourceInputDrift("input alias drift").into());
                }
                input_bytes
                    .get(old_path)
                    .cloned()
                    .context("missing admitted input")?
            } else {
                Some(Arc::from(regular_read(&path, cap, item, cancel, None)?))
            }
        }
    };
    if let Some(key) = state.as_ref().and_then(identity) {
        identities.entry(key).or_insert_with(|| path.clone());
    }
    inputs.insert(path.clone(), state);
    input_bytes.insert(path, bytes);
    Ok(())
}
fn admit_running_executable(
    image: &RunningExecutable,
    cancel: &CancelFlag,
    source_identities: &BTreeSet<(u64, u64)>,
    identities: &mut BTreeMap<(u64, u64), PathBuf>,
    inputs: &mut BTreeMap<PathBuf, Option<Stamp>>,
    input_bytes: &mut BTreeMap<PathBuf, Option<Arc<[u8]>>>,
) -> Result<()> {
    check(cancel)?;
    let state = image.initial.clone();
    let key = identity(&state);
    ensure!(
        !key.is_some_and(|key| source_identities.contains(&key)),
        "running executable aliases source"
    );
    // No other role may claim this path or inode: its bytes live only in the
    // process-start digest, not the per-capture input byte map.
    ensure!(
        !inputs.contains_key(&image.path) && !key.is_some_and(|key| identities.contains_key(&key)),
        "running executable aliases another input"
    );
    image.verify_stat()?;
    if let Some(key) = key {
        identities.insert(key, image.path.clone());
    }
    inputs.insert(image.path.clone(), Some(state));
    input_bytes.insert(image.path.clone(), None);
    Ok(())
}

fn source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|x| x.to_str()),
        Some("js" | "mjs" | "cjs" | "rs" | "java" | "py")
    )
}
/// Directories strictly below `root` that contain a JavaScript source, directly or nested.
fn nested_package_dirs(root: &Path, sources: &[PathBuf]) -> BTreeSet<PathBuf> {
    sources
        .iter()
        .filter(|path| {
            matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("js" | "mjs" | "cjs")
            )
        })
        .flat_map(|path| path.ancestors().skip(1))
        .filter(|dir| dir.starts_with(root) && *dir != root)
        .map(Path::to_path_buf)
        .collect()
}
/// A nested package root admitted for scip-typescript module markers (#443).
pub(crate) fn nested_package_manifest(root: &Path, path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "package.json")
        && path.starts_with(root)
        && path.parent() != Some(root)
}
fn walk(root: &Path, cancel: &CancelFlag) -> Result<(BTreeMap<PathBuf, Stamp>, Vec<PathBuf>)> {
    let mut builder = ignore::WalkBuilder::new(root);
    let filter_root = root.to_owned();
    builder
        .require_git(false)
        .follow_links(false)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            match e.file_name().to_str() {
                Some(".git" | "node_modules" | ".venv" | ".trellis") => false,
                Some("target" | "dist" | "build") => e
                    .path()
                    .strip_prefix(&filter_root)
                    .ok()
                    .is_some_and(|relative| {
                        let parts: Vec<_> = relative.components().collect();
                        parts.windows(3).any(|p| {
                            p[0].as_os_str() == "src"
                                && matches!(
                                    p[1].as_os_str().to_str(),
                                    Some("main" | "test" | "testFixtures")
                                )
                                && p[2].as_os_str() == "java"
                        })
                    }),
                _ => true,
            }
        });
    let mut inventory = BTreeMap::new();
    let mut sources = Vec::new();
    for entry in builder.build() {
        check(cancel)?;
        let entry = entry.context("workspace walk failed")?;
        let path = entry.path();
        let kind = entry.file_type().context("missing entry type")?;
        if kind.is_dir() || source(path) {
            let item = metadata(path)?.context("walk entry disappeared")?;
            if kind.is_dir() {
                ensure!(item.kind == 1, "directory changed: {}", path.display());
            } else {
                ensure!(item.kind == 2, "unsafe source: {}", path.display());
                sources.push(path.to_owned());
            }
            inventory.insert(path.to_owned(), item);
        }
        ensure!(
            sources.len() <= 100_000,
            "workspace exceeds 100000 source files"
        );
    }
    sources.sort();
    Ok((inventory, sources))
}

/// The running image must be pinned before the executable pathname is replaced.
/// The CLI calls this at startup; embedders must do the same before indexing.
/// Library test processes also initialize lazily on their first capture.
pub fn pin_running_executable() -> Result<()> {
    running_executable().map(|_| ())
}

struct RunningExecutable {
    path: PathBuf,
    file: std::sync::Mutex<fs::File>,
    initial: Stamp,
    digest: String,
}

impl RunningExecutable {
    fn open() -> Result<Self> {
        let path = std::env::current_exe()?;
        // procfs opens the executing inode even after an atomic pathname swap.
        // On macOS retain an open descriptor before any later upgrade.
        #[cfg(target_os = "linux")]
        let file = fs::File::open("/proc/self/exe")?;
        #[cfg(not(target_os = "linux"))]
        let file = {
            let mut options = fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            options.open(&path)?
        };
        Self::from_file(path, file)
    }
    fn from_file(path: PathBuf, file: fs::File) -> Result<Self> {
        let initial = stamp(&file.metadata()?);
        ensure!(
            initial.kind == 2 && (1..=512 * 1024 * 1024).contains(&initial.len),
            "unsafe or oversized running executable"
        );
        let mut image = Self {
            path,
            file: std::sync::Mutex::new(file),
            initial,
            digest: String::new(),
        };
        let bytes = image.read_file(&Arc::new(std::sync::atomic::AtomicBool::new(false)))?;
        image.digest = hash(&bytes);
        Ok(image)
    }
    fn verify_stat(&self) -> Result<()> {
        let file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("running executable lock poisoned"))?;
        self.check_file(&file)
    }
    fn read_file(&self, cancel: &CancelFlag) -> Result<Arc<[u8]>> {
        use std::io::{Seek, SeekFrom};
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("running executable lock poisoned"))?;
        self.check_file(&file)?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 65536];
        loop {
            check(cancel)?;
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            ensure!(
                (bytes.len() as u64) + (n as u64) <= 512 * 1024 * 1024,
                "running executable exceeded byte limit"
            );
            bytes.extend_from_slice(&buffer[..n]);
        }
        self.check_file(&file)?;
        ensure!(
            bytes.len() as u64 == self.initial.len,
            "running executable bytes drift"
        );
        Ok(Arc::from(bytes))
    }
    fn check_file(&self, file: &fs::File) -> Result<()> {
        let now = stamp(&file.metadata()?);
        // Unlinking the running inode changes ctime, not the executable bytes.
        // Preserve the initial input observation while checking stable identity
        // and content both at admission and at the final cutoff.
        ensure!(
            now.kind == self.initial.kind && now.len == self.initial.len,
            "running executable metadata drift"
        );
        #[cfg(unix)]
        ensure!(
            now.dev == self.initial.dev
                && now.ino == self.initial.ino
                && now.mtime == self.initial.mtime,
            "running executable inode or mtime drift"
        );
        Ok(())
    }
}

static RUNNING_EXECUTABLE: std::sync::OnceLock<
    std::result::Result<Arc<RunningExecutable>, String>,
> = std::sync::OnceLock::new();

fn running_executable() -> Result<Arc<RunningExecutable>> {
    RUNNING_EXECUTABLE
        .get_or_init(|| {
            RunningExecutable::open()
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| anyhow::anyhow!(message.clone()))
}

#[derive(Debug)]
pub(crate) struct SourceInputDrift(pub(crate) &'static str);
impl std::fmt::Display for SourceInputDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for SourceInputDrift {}

/// A changed source inventory invalidates only this captured snapshot. A FIFO
/// owner may retry the same accepted row after proving no publication committed.
#[derive(Debug)]
pub(crate) struct WorkspaceInventoryDrift;
impl std::fmt::Display for WorkspaceInventoryDrift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("workspace inventory drift")
    }
}
impl std::error::Error for WorkspaceInventoryDrift {}

/// Bytes and complete admission inventory are retained until the final cutoff.
/// No source is physically opened again during validation.
pub struct Capture {
    admission_root: PathBuf,
    root_stamp: Stamp,
    root: PathBuf,
    inventory: BTreeMap<PathBuf, Stamp>,
    inputs: BTreeMap<PathBuf, Option<Stamp>>,
    pub files: Vec<SourceFile>,
    pub source_operations: BTreeMap<String, SourceOperations>,
    graph_projections: AtomicUsize,
    pub hashes: BTreeMap<String, String>,
    // One admitted immutable executable byte allocation and one SHA-256 pass.
    // The private identity binds the cached digest to this exact captured Arc.
    executable_path: PathBuf,
    executable_bytes: Option<Arc<[u8]>>,
    executable_digest: String,
    running_executable: Option<Arc<RunningExecutable>>,
    input_bytes: BTreeMap<PathBuf, Option<Arc<[u8]>>>,
    reconcile_options: crate::indexer::ReconcileOptions,
}
impl Capture {
    pub fn admit(
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: &impl Fn(IndexProgress),
    ) -> Result<Self> {
        let running = running_executable()?;
        Self::admit_with_image(
            options,
            cancel,
            progress,
            running.path.clone(),
            Some(running),
        )
    }
    #[cfg(test)]
    fn admit_with_executable(
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: &impl Fn(IndexProgress),
        exe: PathBuf,
    ) -> Result<Self> {
        Self::admit_with_image(options, cancel, progress, exe, None)
    }
    fn admit_with_image(
        options: &IndexOptions,
        cancel: &CancelFlag,
        progress: &impl Fn(IndexProgress),
        exe: PathBuf,
        running_executable: Option<Arc<RunningExecutable>>,
    ) -> Result<Self> {
        check(cancel)?;
        let root_meta = metadata(&options.workspace_root)?.context("workspace root absent")?;
        ensure!(
            root_meta.kind == 1,
            "workspace root is not a real directory"
        );
        let root = fs::canonicalize(&options.workspace_root)?;
        let (inventory, sources) = walk(&root, cancel)?;
        ensure!(
            !running_executable.is_some() || !sources.contains(&exe),
            "running executable pathname conflicts with source"
        );
        let mut inputs = BTreeMap::new();
        let mut input_bytes = BTreeMap::new();
        let mut source_identities = BTreeSet::new();
        let mut input_identities = BTreeMap::new();
        for path in &sources {
            let item = inventory.get(path).context("missing source inventory")?;
            if let Some(key) = identity(item) {
                ensure!(
                    source_identities.insert(key),
                    "duplicate source identity: {}",
                    path.display()
                );
            }
        }
        for name in ROOT_INPUTS {
            admit_input(
                root.join(name),
                options.max_file_bytes.min(16 * 1024 * 1024),
                cancel,
                &source_identities,
                &mut input_identities,
                &mut inputs,
                &mut input_bytes,
            )?;
        }
        for dir in inventory
            .iter()
            .filter(|(_, s)| s.kind == 1)
            .map(|(p, _)| p)
        {
            for name in [".gitignore", ".ignore"] {
                admit_input(
                    dir.join(name),
                    options.max_file_bytes.min(16 * 1024 * 1024),
                    cancel,
                    &source_identities,
                    &mut input_identities,
                    &mut inputs,
                    &mut input_bytes,
                )?;
            }
        }
        // scip-typescript names a module relative to the nearest package.json directory, so
        // nested package roots of JavaScript sources are admitted beside the root inputs
        // (owner ruling 2026-10-29, #443). Like the producer, only a regular file counts.
        for dir in nested_package_dirs(&root, &sources) {
            let path = dir.join("package.json");
            let cap = options.max_file_bytes.min(16 * 1024 * 1024);
            if metadata(&path)?.is_some_and(|item| item.kind != 2 || item.len > cap) {
                continue;
            }
            admit_input(
                path,
                cap,
                cancel,
                &source_identities,
                &mut input_identities,
                &mut inputs,
                &mut input_bytes,
            )?;
        }
        // The linked native extractor is part of the running executable, not ambient PATH.
        if let Some(image) = running_executable.as_ref() {
            admit_running_executable(
                image,
                cancel,
                &source_identities,
                &mut input_identities,
                &mut inputs,
                &mut input_bytes,
            )?;
        } else {
            admit_input(
                exe.clone(),
                512 * 1024 * 1024,
                cancel,
                &source_identities,
                &mut input_identities,
                &mut inputs,
                &mut input_bytes,
            )?;
        }
        let executable_bytes = if running_executable.is_some() {
            None
        } else {
            Some(
                input_bytes
                    .get(&exe)
                    .and_then(Option::as_ref)
                    .filter(|bytes| !bytes.is_empty())
                    .cloned()
                    .with_context(|| {
                        format!("running executable bytes unavailable: {}", exe.display())
                    })?,
            )
        };
        check(cancel)?;
        let executable_digest = if let Some(image) = running_executable.as_ref() {
            image.digest.clone()
        } else {
            hash(executable_bytes.as_ref().unwrap())
        };
        check(cancel)?;
        let mut files = Vec::new();
        let mut hashes = BTreeMap::new();
        let mut source_operations = BTreeMap::new();
        let mut total_bytes = 0u64;
        for (i, path) in sources.iter().enumerate() {
            check(cancel)?;
            let state = inventory
                .get(path)
                .context("source absent from inventory")?;
            let mut counts = SourceOperations::default();
            let bytes = regular_read(
                path,
                options.max_file_bytes.min(256 * 1024 * 1024),
                state,
                cancel,
                Some(&mut counts),
            )?;
            total_bytes += bytes.len() as u64;
            ensure!(
                total_bytes <= 256 * 1024 * 1024,
                "workspace source exceeds 256 MiB"
            );
            let rel = path
                .strip_prefix(&root)?
                .to_str()
                .context("non-UTF8 source path")?
                .replace('\\', "/");
            let text = String::from_utf8(bytes).context("non-UTF8 source text")?;
            let digest = hash(text.as_bytes());
            counts.hashes += 1;
            source_operations.insert(rel.clone(), counts);
            hashes.insert(rel.clone(), digest.clone());
            files.push(SourceFile {
                path: rel,
                hash: digest,
                language: match path.extension().and_then(|e| e.to_str()) {
                    Some("rs") => "rust",
                    Some("java") => "java",
                    Some("py") => "python",
                    _ => "javascript",
                }
                .into(),
                text,
            });
            progress(IndexProgress {
                phase: "scan".into(),
                completed: i + 1,
                total: sources.len(),
            });
        }
        let capture = Self {
            admission_root: options.workspace_root.clone(),
            root_stamp: root_meta,
            root,
            inventory,
            inputs,
            files,
            source_operations,
            graph_projections: AtomicUsize::new(0),
            hashes,
            executable_path: exe,
            executable_bytes,
            executable_digest,
            running_executable,
            input_bytes,
            reconcile_options: crate::indexer::ReconcileOptions::from(options),
        };
        capture.verify(cancel)?;
        Ok(capture)
    }
    /// Match an already verified capture to the watcher options and request root.
    /// The coordinator supplies its fresh admission or verified publication capture;
    /// this identity check must not repeat the capture's full inventory verification.
    pub(crate) fn covers_watcher_request(
        &self,
        options: &IndexOptions,
        root_device: &str,
        root_inode: &str,
    ) -> bool {
        #[cfg(unix)]
        if self.root_stamp.dev.to_string() != root_device
            || self.root_stamp.ino.to_string() != root_inode
        {
            return false;
        }
        #[cfg(not(unix))]
        let _ = (root_device, root_inode);
        (self.admission_root == options.workspace_root
            || std::fs::canonicalize(&options.workspace_root).ok() == Some(self.root.clone()))
            && self.reconcile_options == crate::indexer::ReconcileOptions::from(options)
    }

    /// A capture may feed exactly one graph projection. Cross-link validation is not projection.
    pub fn graph_projection_count(&self) -> usize {
        self.graph_projections.load(Ordering::Relaxed)
    }
    pub(crate) fn claim_graph_projection(&self) -> Result<()> {
        ensure!(
            self.graph_projections.fetch_add(1, Ordering::Relaxed) == 0,
            "a capture cannot project a graph twice"
        );
        Ok(())
    }
    pub fn verify(&self, cancel: &CancelFlag) -> Result<()> {
        check(cancel)?;
        ensure!(
            metadata(&self.admission_root)?.as_ref() == Some(&self.root_stamp),
            "workspace root drift"
        );
        let (inventory, _) = walk(&self.root, cancel)?;
        if inventory != self.inventory {
            return Err(WorkspaceInventoryDrift.into());
        }
        for (path, expected) in &self.inputs {
            check(cancel)?;
            if path == &self.executable_path
                && let Some(image) = self.running_executable.as_ref()
            {
                ensure!(
                    path == &image.path && expected.as_ref() == Some(&image.initial),
                    "running executable observation drift"
                );
                ensure!(
                    self.input_bytes.get(path) == Some(&None),
                    "running executable input bytes unexpectedly populated"
                );
                image.verify_stat()?;
                continue;
            }
            if &metadata(path)? != expected {
                return Err(SourceInputDrift("input inventory drift").into());
            }
            // Relevant config, manifest, ignore and toolchain bytes can change
            // while a filesystem reports an unchanged stat. Do not infer their
            // equality from timestamps at the publication cutoff.
            if let Some(state) = expected {
                let now = regular_read(path, self.input_cap(path), state, cancel, None)?;
                if self.input_bytes.get(path).and_then(Option::as_deref) != Some(now.as_slice()) {
                    return Err(SourceInputDrift("input contents drift").into());
                }
            }
        }
        check(cancel)?;
        Ok(())
    }
    fn input_cap(&self, path: &Path) -> u64 {
        if path == self.executable_path {
            512 * 1024 * 1024
        } else {
            16 * 1024 * 1024
        }
    }
    pub(crate) fn physical_root(&self) -> &Path {
        &self.root
    }
    pub fn bytes(&self, path: &Path) -> Option<&[u8]> {
        self.input_bytes.get(path).and_then(|b| b.as_deref())
    }
    /// Stable process-start identity, not the pathname's later replacement.
    pub(crate) fn executable_path(&self) -> &Path {
        &self.executable_path
    }
    /// The executing image is backed by a startup-pinned descriptor and digest.
    /// Fixture-only path admissions still require their exact byte Arc.
    pub(crate) fn executable_digest(&self, path: &Path) -> Option<&str> {
        if path != self.executable_path.as_path() {
            return None;
        }
        let admitted = if let Some(image) = self.running_executable.as_ref() {
            self.inputs.get(path) == Some(&Some(image.initial.clone()))
                && self.input_bytes.get(path) == Some(&None)
                && self.executable_bytes.is_none()
                && self.executable_digest == image.digest
        } else {
            self.input_bytes
                .get(path)
                .and_then(Option::as_ref)
                .zip(self.executable_bytes.as_ref())
                .is_some_and(|(entry, original)| Arc::ptr_eq(entry, original))
        };
        admitted.then_some(self.executable_digest.as_str())
    }
    /// Fingerprint native inputs by their admitted roles. The executable uses
    /// its admitted Arc digest; do not rehash its bytes.
    pub(crate) fn native_input_fingerprints(&self) -> Result<BTreeMap<String, Option<String>>> {
        let mut result = BTreeMap::new();
        for (path, expected) in &self.inputs {
            let mut roles = Vec::new();
            if path == &self.executable_path {
                roles.push(format!("executable:{}", path.to_string_lossy()));
            }
            if path.starts_with(&self.root) {
                let relative = path
                    .strip_prefix(&self.root)?
                    .to_str()
                    .context("non-UTF8 native input path")?
                    .replace('\\', "/");
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".gitignore" | ".ignore")
                ) {
                    roles.push(format!("ignore:{relative}"));
                } else if matches!(relative.as_str(), "rust-toolchain" | "rust-toolchain.toml") {
                    roles.push(format!("toolchain:{relative}"));
                } else if ROOT_INPUTS.contains(&relative.as_str())
                    || nested_package_manifest(&self.root, path)
                {
                    roles.push(format!("config:{relative}"));
                }
            }
            if roles.is_empty() {
                continue;
            }
            let bytes = self
                .input_bytes
                .get(path)
                .context("missing admitted native input")?;
            let digest = match (expected, bytes) {
                (None, None) => None,
                (Some(stat), None)
                    if path == &self.executable_path && self.running_executable.is_some() =>
                {
                    ensure!(stat.kind == 2, "malformed admitted native executable");
                    Some(
                        self.executable_digest(path)
                            .context("native executable startup identity mismatch")?
                            .to_owned(),
                    )
                }
                (Some(stat), Some(bytes)) => {
                    ensure!(
                        stat.kind == 2 && stat.len == bytes.len() as u64,
                        "malformed admitted native input"
                    );
                    Some(if path == &self.executable_path {
                        self.executable_digest(path)
                            .context("native executable Arc identity mismatch")?
                            .to_owned()
                    } else {
                        hash(bytes)
                    })
                }
                _ => anyhow::bail!("native input bytes/stamp mismatch"),
            };
            for role in roles {
                ensure!(
                    result.insert(role, digest.clone()).is_none(),
                    "duplicate admitted native input role"
                );
            }
        }
        ensure!(
            result.keys().any(|key| key.starts_with("executable:")),
            "missing admitted native executable role"
        );
        ensure!(
            ROOT_INPUTS.iter().all(|name| {
                result.contains_key(&format!(
                    "{}:{name}",
                    if matches!(*name, "rust-toolchain" | "rust-toolchain.toml") {
                        "toolchain"
                    } else {
                        "config"
                    }
                ))
            }),
            "missing admitted native config/toolchain role"
        );
        Ok(result)
    }

    pub(crate) fn admitted_inputs(&self) -> impl Iterator<Item = (&Path, Option<&[u8]>)> {
        self.input_bytes
            .iter()
            .map(|(path, bytes)| (path.as_path(), bytes.as_deref()))
    }
    pub(crate) fn reconcile_options(&self) -> &crate::indexer::ReconcileOptions {
        &self.reconcile_options
    }
    pub(crate) fn source_stat(&self, relative: &str) -> Result<CaptureStat> {
        let path = self.root.join(relative);
        self.inventory
            .get(&path)
            .map(Stamp::persisted)
            .context("source absent from capture inventory")
    }
    pub(crate) fn persisted_inputs(&self) -> Result<BTreeMap<String, CaptureInputObservation>> {
        fn relative(root: &Path, path: &Path) -> Result<String> {
            Ok(path
                .strip_prefix(root)?
                .to_str()
                .context("non-UTF8 input path")?
                .replace('\\', "/"))
        }
        let mut result = BTreeMap::new();
        result.insert(
            "root:.".into(),
            CaptureInputObservation::Root {
                stat: self.root_stamp.persisted(),
            },
        );
        for (path, item) in &self.inventory {
            if item.kind == 1 {
                result.insert(
                    format!("directory:{}", relative(&self.root, path)?),
                    CaptureInputObservation::Directory {
                        stat: item.persisted(),
                    },
                );
            }
        }
        for (path, state) in &self.inputs {
            let mut roles = Vec::new();
            if path == &self.executable_path {
                roles.push(format!("executable:{}", path.to_string_lossy()));
            }
            if path.starts_with(&self.root) {
                let rel = relative(&self.root, path)?;
                if matches!(
                    path.file_name().and_then(|v| v.to_str()),
                    Some(".gitignore" | ".ignore")
                ) {
                    roles.push(format!("ignore:{rel}"));
                } else if matches!(rel.as_str(), "rust-toolchain" | "rust-toolchain.toml") {
                    roles.push(format!("toolchain:{rel}"));
                } else if ROOT_INPUTS.contains(&rel.as_str())
                    || nested_package_manifest(&self.root, path)
                {
                    roles.push(format!("config:{rel}"));
                }
            }
            let observation = match state {
                None => CaptureInputObservation::Absent,
                Some(stat) => CaptureInputObservation::Present {
                    stat: stat.persisted(),
                    hash: if path == &self.executable_path && self.running_executable.is_some() {
                        self.executable_digest(path)
                            .context("running executable startup identity missing")?
                            .to_owned()
                    } else {
                        hash(
                            self.input_bytes
                                .get(path)
                                .and_then(Option::as_deref)
                                .context("present input bytes missing")?,
                        )
                    },
                },
            };
            ensure!(!roles.is_empty(), "capture input lacks a role");
            for role in roles {
                ensure!(
                    result.insert(role, observation.clone()).is_none(),
                    "duplicate capture input role"
                );
            }
        }
        // The producer hash of the extraction code this executable runs. A
        // rebuilt executable with an equal hash keeps the selected facts.
        result.insert(
            EXTRACTOR_INPUT_KEY.into(),
            CaptureInputObservation::Extractor {
                hash: crate::native_evidence::extractor_hash(),
            },
        );
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    #[test]
    fn verified_capture_matches_only_its_watcher_options_and_request_root() {
        let workspace = tempfile::tempdir().unwrap();
        let exe = tempfile::tempdir().unwrap();
        let binary = exe.path().join("extractor");
        fs::write(&binary, b"binary").unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit_with_executable(&options, &cancel, &|_| {}, binary).unwrap();
        #[cfg(unix)]
        let (device, inode) = (
            capture.root_stamp.dev.to_string(),
            capture.root_stamp.ino.to_string(),
        );
        #[cfg(not(unix))]
        let (device, inode) = (String::new(), String::new());

        assert!(capture.covers_watcher_request(&options, &device, &inode));
        let mut different = options.clone();
        different.max_file_bytes /= 2;
        assert!(!capture.covers_watcher_request(&different, &device, &inode));
        let other_root = tempfile::tempdir().unwrap();
        different = options.clone();
        different.workspace_root = other_root.path().to_owned();
        assert!(!capture.covers_watcher_request(&different, &device, &inode));
        #[cfg(unix)]
        {
            assert!(!capture.covers_watcher_request(&options, "different", &inode));
            assert!(!capture.covers_watcher_request(&options, &device, "different"));
        }
    }

    #[test]
    fn input_inventory_drift_is_typed_without_source_inventory_change() {
        let workspace = tempfile::tempdir().unwrap();
        let exe = tempfile::tempdir().unwrap();
        let binary = exe.path().join("extractor");
        fs::write(&binary, b"binary").unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let manifest = workspace.path().join("package.json");
        fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let changed = AtomicBool::new(false);
        let error = Capture::admit_with_executable(
            &options,
            &cancel,
            &|progress| {
                if progress.phase == "scan" && !changed.swap(true, Ordering::AcqRel) {
                    fs::write(&manifest, "{\"name\":\"after\"}\n").unwrap();
                }
            },
            binary,
        )
        .err()
        .unwrap();
        assert!(changed.load(Ordering::Acquire));
        assert_eq!(
            error.downcast_ref::<SourceInputDrift>().unwrap().0,
            "input inventory drift"
        );
    }

    #[cfg(unix)]
    #[test]
    fn input_alias_drift_is_typed() {
        let workspace = tempfile::tempdir().unwrap();
        let exe = tempfile::tempdir().unwrap();
        let binary = exe.path().join("extractor");
        fs::write(&binary, b"binary").unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let manifest = workspace.path().join("package.json");
        fs::write(&manifest, "{\"name\":\"before\"}\n").unwrap();
        fs::hard_link(&manifest, workspace.path().join("tsconfig.json")).unwrap();
        let canonical = fs::canonicalize(&manifest).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let error = with_source_read_hook(
            move |path, phase| {
                if path == canonical && phase == "alias" {
                    let replacement = canonical.with_extension("replacement");
                    fs::write(&replacement, "{\"name\":\"after\"}\n").unwrap();
                    fs::rename(replacement, &canonical).unwrap();
                }
            },
            || Capture::admit_with_executable(&options, &cancel, &|_| {}, binary),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.downcast_ref::<SourceInputDrift>().unwrap().0,
            "input alias drift"
        );
    }

    #[test]
    fn same_stat_input_bytes_mismatch_is_typed_contents_drift() {
        let workspace = tempfile::tempdir().unwrap();
        let exe = tempfile::tempdir().unwrap();
        let binary = exe.path().join("extractor");
        fs::write(&binary, b"binary").unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let manifest = workspace.path().join("package.json");
        fs::write(&manifest, "{\"name\":\"same\"}\n").unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut capture =
            Capture::admit_with_executable(&options, &cancel, &|_| {}, binary).unwrap();
        let key = fs::canonicalize(workspace.path())
            .unwrap()
            .join("package.json");
        capture
            .input_bytes
            .insert(key, Some(Arc::from(b"different cached bytes".as_slice())));
        let error = capture.verify(&cancel).unwrap_err();
        assert_eq!(
            error.downcast_ref::<SourceInputDrift>().unwrap().0,
            "input contents drift"
        );
    }

    #[test]
    fn genuine_open_error_with_unchanged_input_stamp_is_not_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        fs::write(&path, "{}\n").unwrap();
        let expected = metadata(&path).unwrap().unwrap();
        let error = open_error(
            &path,
            &expected,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<SourceInputDrift>().is_none());
        assert_eq!(
            error
                .root_cause()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn pinned_executable_keeps_running_inode_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("native-bin");
        fs::write(&live, b"old executing binary").unwrap();
        let image =
            RunningExecutable::from_file(live.clone(), fs::File::open(&live).unwrap()).unwrap();
        let startup_digest = hash(b"old executing binary");
        assert_eq!(image.digest, startup_digest);
        image.verify_stat().unwrap();
        fs::write(dir.path().join("replacement"), b"new pathname binary").unwrap();
        fs::rename(dir.path().join("replacement"), &live).unwrap();
        image.verify_stat().unwrap();
        assert_eq!(image.digest, startup_digest);
        assert_eq!(fs::read(&live).unwrap(), b"new pathname binary");
    }
    #[test]
    fn pinned_executable_refuses_in_place_stat_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-bin");
        fs::write(&path, b"old executing binary").unwrap();
        let image =
            RunningExecutable::from_file(path.clone(), fs::File::open(&path).unwrap()).unwrap();
        fs::write(&path, b"new larger executable binary").unwrap();
        let error = image.verify_stat().unwrap_err();
        assert!(error.downcast_ref::<SourceInputDrift>().is_none());
        assert!(
            error.to_string().contains("executable"),
            "changing the pinned inode must fail its stat check: {error:#}"
        );
    }
    #[test]
    fn missing_executable_refuses_admission() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one.js"), "f();").unwrap();
        let missing = root.path().join("not-running-bin");
        let cancel = Arc::new(AtomicBool::new(false));
        let error = Capture::admit_with_executable(
            &IndexOptions::new(root.path().to_owned()),
            &cancel,
            &|_| {},
            missing,
        )
        .err()
        .expect("missing executable must fail admission");
        assert!(
            error.to_string().contains("running executable"),
            "{error:#}"
        );
    }
    #[test]
    fn nonregular_or_empty_executable_refuses_admission() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("one.js"), "f();").unwrap();
        let exe = root.path().join("fake-native-bin");
        fs::write(&exe, "").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(
            Capture::admit_with_executable(
                &IndexOptions::new(root.path().to_owned()),
                &cancel,
                &|_| {},
                exe.clone(),
            )
            .is_err()
        );
        fs::remove_file(&exe).unwrap();
        std::os::unix::fs::symlink(root.path().join("one.js"), &exe).unwrap();
        assert!(
            Capture::admit_with_executable(
                &IndexOptions::new(root.path().to_owned()),
                &cancel,
                &|_| {},
                exe,
            )
            .is_err()
        );
    }
    #[test]
    fn executable_cutoff_and_stable_source_operation_counts() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("fake-native-bin");
        fs::write(&exe, "native-v1").unwrap();
        fs::write(root.path().join("one.js"), "f();").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit_with_executable(
            &IndexOptions::new(root.path().to_owned()),
            &cancel,
            &|_| {},
            exe.clone(),
        )
        .unwrap();
        assert_eq!(
            capture.source_operations["one.js"],
            SourceOperations {
                opens: 1,
                complete_reads: 1,
                hashes: 1,
            }
        );
        assert_eq!(capture.bytes(&exe), Some(&b"native-v1"[..]));
        assert_eq!(
            capture.executable_digest(&exe),
            Some("d0f25cc40fe46416cf32b2e565dd9a5fbd979845a57297a3c78e0fc833048ba5")
        );
        capture.verify(&cancel).unwrap();
        fs::write(&exe, "native-v2").unwrap();
        assert!(
            capture
                .verify(&cancel)
                .unwrap_err()
                .to_string()
                .contains("drift")
        );
    }
    #[test]
    fn cached_executable_digest_rejects_wrong_path_and_replaced_arc() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("fake-native-bin");
        fs::write(&exe, "native-v1").unwrap();
        fs::write(root.path().join("one.js"), "f();").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut capture = Capture::admit_with_executable(
            &IndexOptions::new(root.path().to_owned()),
            &cancel,
            &|_| {},
            exe.clone(),
        )
        .unwrap();
        let admitted = capture
            .input_bytes
            .get(&exe)
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        assert!(Arc::ptr_eq(
            &admitted,
            capture.executable_bytes.as_ref().unwrap()
        ));
        let digest = capture.executable_digest(&exe).unwrap().to_owned();
        assert_eq!(
            digest,
            "d0f25cc40fe46416cf32b2e565dd9a5fbd979845a57297a3c78e0fc833048ba5"
        );
        let wrong = root.path().join("forged-native-bin");
        capture
            .input_bytes
            .insert(wrong.clone(), Some(admitted.clone()));
        assert_eq!(
            capture.executable_digest(&wrong),
            None,
            "even the same Arc under a wrong executable path is not admitted"
        );
        let equal_bytes: Arc<[u8]> = Arc::from(admitted.as_ref());
        assert!(!Arc::ptr_eq(&admitted, &equal_bytes));
        capture.input_bytes.insert(exe.clone(), Some(equal_bytes));
        assert_eq!(
            capture.executable_digest(&exe),
            None,
            "forged equal bytes in a different Arc must fail closed"
        );
        capture
            .input_bytes
            .insert(exe.clone(), Some(Arc::from(&b"native-v2"[..])));
        assert_eq!(
            capture.executable_digest(&exe),
            None,
            "forged different bytes must fail closed"
        );
        capture.input_bytes.remove(&exe);
        assert_eq!(
            capture.executable_digest(&exe),
            None,
            "removed executable bytes must fail closed"
        );
        capture.input_bytes.insert(exe.clone(), Some(admitted));
        assert_eq!(capture.executable_digest(&exe), Some(digest.as_str()));
        capture.verify(&cancel).unwrap();
    }
    #[test]
    fn required_inputs_and_uncertain_sources_take_unconditional_hash_path() {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("fake-native-bin");
        fs::write(&exe, "native-v1").unwrap();
        fs::write(root.path().join("one.js"), "f();").unwrap();
        fs::write(root.path().join("package.json"), "{\"a\":1}").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let options = IndexOptions::new(root.path().to_owned());
        let first =
            Capture::admit_with_executable(&options, &cancel, &|_| {}, exe.clone()).unwrap();
        fs::write(root.path().join("one.js"), "g();").unwrap();
        fs::write(root.path().join("package.json"), "{\"b\":2}").unwrap();
        let second = Capture::admit_with_executable(&options, &cancel, &|_| {}, exe).unwrap();

        let mut previous_inputs = first.persisted_inputs().unwrap();
        let mut current_inputs = second.persisted_inputs().unwrap();
        let key = "config:package.json";
        previous_inputs.retain(|candidate, _| candidate == key);
        current_inputs.retain(|candidate, _| candidate == key);
        let old_stat = match &previous_inputs[key] {
            CaptureInputObservation::Present { stat, .. } => stat.clone(),
            other => panic!("required config was not captured: {other:?}"),
        };
        match current_inputs.get_mut(key).unwrap() {
            CaptureInputObservation::Present { stat, .. } => *stat = old_stat,
            other => panic!("required config was not recaptured: {other:?}"),
        }
        let input_comparison = crate::store::compare_capture_observations(
            &BTreeMap::new(),
            &BTreeMap::new(),
            &previous_inputs,
            &current_inputs,
        );
        assert_eq!(input_comparison.stat_changed, 0);
        assert_eq!(input_comparison.hash_changed, 1);
        assert_eq!(input_comparison.unchanged_stat_input_hash_changed, 1);

        let mut uncertain = second.source_stat("one.js").unwrap();
        uncertain.mtime_seconds = None;
        uncertain.mtime_nanoseconds = None;
        uncertain.ctime_seconds = None;
        uncertain.ctime_nanoseconds = None;
        let previous_sources = BTreeMap::from([(
            "one.js".to_owned(),
            (first.files[0].hash.clone(), uncertain.clone()),
        )]);
        let current_sources = BTreeMap::from([(
            "one.js".to_owned(),
            (second.files[0].hash.clone(), uncertain),
        )]);
        let uncertain_comparison = crate::store::compare_capture_observations(
            &previous_sources,
            &current_sources,
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(uncertain_comparison.stat_changed, 0);
        assert_eq!(uncertain_comparison.hash_changed, 1);
        assert_eq!(uncertain_comparison.uncertain_timestamp_hashed, 1);
        assert_ne!(first.files[0].text, second.files[0].text);
        assert_eq!(
            second.source_operations["one.js"],
            SourceOperations {
                opens: 1,
                complete_reads: 1,
                hashes: 1,
            }
        );
    }
}
