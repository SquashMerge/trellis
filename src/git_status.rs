//! Read-only Git measurements. Synchronous libgit2 reads run only in a bounded
//! instance of our own executable, not in the daemon's async executor.
use git2::{BranchType, Delta, DiffFindOptions, DiffOptions, ErrorCode, Repository};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
};

pub const READ_DEADLINE: Duration = Duration::from_secs(5);
pub const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitReason {
    NotRepository,
    Unreadable,
    InvalidRepository,
    UnbornHead,
    TrackingUnavailable,
    Conflicted,
    UnsupportedPath,
    DeadlineExceeded,
    ResponseTooLarge,
    GitChanged,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum GitSummary {
    Available {
        head: GitHead,
        upstream: Option<GitUpstream>,
        #[serde(rename = "workingTree")]
        working_tree: GitWorkingTree,
    },
    Unavailable {
        reason: GitReason,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum GitWorkingTree {
    Available {
        #[serde(rename = "changedFiles")]
        changed_files: usize,
    },
    Unavailable {
        reason: GitReason,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHead {
    pub kind: GitHeadKind,
    pub name: Option<String>,
    pub commit: String,
    pub short_commit: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitHeadKind {
    Branch,
    Detached,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitUpstream {
    pub name: String,
    pub ahead: usize,
    pub behind: usize,
    pub basis: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    pub git: GitSummary,
    pub changes: Option<Vec<GitChange>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitChange {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: GitChangeKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiff {
    pub git: GitSummary,
    pub file: Option<GitFileDiff>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitFileDiff {
    pub path: String,
    pub previous_path: Option<String>,
    pub status: GitChangeKind,
    pub binary: bool,
    pub hunks: Vec<GitHunk>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<GitLine>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitLine {
    pub kind: GitLineKind,
    pub text: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitLineKind {
    Context,
    Added,
    Deleted,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitRequestError {
    InvalidPath,
    NotFound,
}

pub fn validate_path(path: &str) -> Result<(), GitRequestError> {
    if path.is_empty()
        || path.contains(['\0', '\\'])
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path
            .split('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        return Err(GitRequestError::InvalidPath);
    }
    Ok(())
}
fn unavailable(reason: GitReason) -> GitSummary {
    GitSummary::Unavailable { reason }
}

/// The trusted Trellis executable used to isolate library reads. This is not a
/// command supplied by the repository; callers can provide the installed binary.
#[derive(Debug, Clone)]
pub struct GitReader {
    executable: PathBuf,
}
impl GitReader {
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
    pub async fn summary(&self, root: &Path) -> GitSummary {
        let head = match self.read(root, "head", None).await {
            Ok(WorkerResponse::Head(Ok(value))) => value,
            Ok(WorkerResponse::Head(Err(reason))) | Err(reason) => return unavailable(reason),
            _ => return unavailable(GitReason::InvalidRepository),
        };
        let reason = match self.read(root, "summary", None).await {
            Ok(WorkerResponse::Summary(GitSummary::Available {
                head: measured_head,
                upstream,
                working_tree,
            })) => {
                if measured_head == head.head && upstream == head.upstream {
                    return head.with_working_tree(working_tree);
                }
                GitReason::GitChanged
            }
            Ok(WorkerResponse::Summary(GitSummary::Unavailable { reason })) | Err(reason) => reason,
            _ => GitReason::InvalidRepository,
        };
        head.with_working_tree(GitWorkingTree::Unavailable { reason })
    }
    pub async fn status(&self, root: &Path) -> GitStatus {
        match self.read(root, "status", None).await {
            Ok(WorkerResponse::Status(value)) => value,
            Err(reason) => GitStatus {
                git: unavailable(reason),
                changes: None,
            },
            _ => GitStatus {
                git: unavailable(GitReason::InvalidRepository),
                changes: None,
            },
        }
    }
    pub async fn diff(&self, root: &Path, path: &str) -> Result<GitDiff, GitRequestError> {
        validate_path(path)?;
        match self.read(root, "diff", Some(path)).await {
            Ok(WorkerResponse::Diff(value)) => value,
            Err(reason) => Ok(GitDiff {
                git: unavailable(reason),
                file: None,
            }),
            _ => Ok(GitDiff {
                git: unavailable(GitReason::InvalidRepository),
                file: None,
            }),
        }
    }
    async fn read(
        &self,
        root: &Path,
        mode: &str,
        path: Option<&str>,
    ) -> Result<WorkerResponse, GitReason> {
        let mut command = Command::new(&self.executable);
        command
            .arg("git-read")
            .arg("--root")
            .arg(root)
            .arg("--mode")
            .arg(mode)
            .env_clear()
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = path {
            command.arg("--path").arg(path);
        }
        let child = command.spawn().map_err(|_| GitReason::Unreadable)?;
        let mut guard = ChildGuard(Some(child));
        let result = tokio::time::timeout(READ_DEADLINE, async {
            let child = guard.0.as_mut().ok_or(GitReason::Unreadable)?;
            let stdout = child.stdout.take().ok_or(GitReason::Unreadable)?;
            let mut bytes = Vec::new();
            stdout
                .take((RESPONSE_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| GitReason::Unreadable)?;
            if bytes.len() > RESPONSE_LIMIT {
                return Err(GitReason::ResponseTooLarge);
            }
            if !child
                .wait()
                .await
                .map_err(|_| GitReason::Unreadable)?
                .success()
            {
                return Err(GitReason::InvalidRepository);
            }
            serde_json::from_slice(&bytes).map_err(|_| GitReason::InvalidRepository)
        })
        .await;
        let result = result.unwrap_or(Err(GitReason::DeadlineExceeded));
        if result.is_err()
            && let Some(child) = guard.0.as_mut()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        result
    }
}
struct ChildGuard(Option<Child>);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take()
            && child.try_wait().ok().flatten().is_none()
        {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}
pub async fn summary(root: &Path) -> GitSummary {
    match std::env::current_exe() {
        Ok(executable) => GitReader::new(executable).summary(root).await,
        Err(_) => unavailable(GitReason::Unreadable),
    }
}
pub async fn status(root: &Path) -> GitStatus {
    match std::env::current_exe() {
        Ok(executable) => GitReader::new(executable).status(root).await,
        Err(_) => GitStatus {
            git: unavailable(GitReason::Unreadable),
            changes: None,
        },
    }
}
pub async fn diff(root: &Path, path: &str) -> Result<GitDiff, GitRequestError> {
    validate_path(path)?;
    match std::env::current_exe() {
        Ok(executable) => GitReader::new(executable).diff(root, path).await,
        Err(_) => Ok(GitDiff {
            git: unavailable(GitReason::Unreadable),
            file: None,
        }),
    }
}

#[doc(hidden)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "mode", content = "result", rename_all = "lowercase")]
pub enum WorkerResponse {
    Head(Result<HeadSummary, GitReason>),
    Summary(GitSummary),
    Status(GitStatus),
    Diff(Result<GitDiff, GitRequestError>),
}
fn failed(mode: &str, reason: GitReason) -> WorkerResponse {
    match mode {
        "head" => WorkerResponse::Head(Err(reason)),
        "summary" => WorkerResponse::Summary(unavailable(reason)),
        "diff" => WorkerResponse::Diff(Ok(GitDiff {
            git: unavailable(reason),
            file: None,
        })),
        _ => WorkerResponse::Status(GitStatus {
            git: unavailable(reason),
            changes: None,
        }),
    }
}
/// Only the hidden own-binary command calls this synchronous library worker.
#[doc(hidden)]
pub fn read_in_child(root: &Path, mode: &str, path: Option<&str>) -> WorkerResponse {
    if mode == "diff" && path.is_none_or(|value| validate_path(value).is_err()) {
        return WorkerResponse::Diff(Err(GitRequestError::InvalidPath));
    }
    match measure(root, mode, path) {
        Ok(response) => response,
        Err(reason) => failed(mode, reason),
    }
}
struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > RESPONSE_LIMIT {
            return Err(io::Error::other("Git response limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[doc(hidden)]
pub fn write_child_response(root: &Path, mode: &str, path: Option<&str>) -> io::Result<()> {
    let response = read_in_child(root, mode, path);
    let mut bytes = BoundedOutput(Vec::new());
    if serde_json::to_writer(&mut bytes, &response).is_err() {
        bytes.0.clear();
        serde_json::to_writer(&mut bytes, &failed(mode, GitReason::ResponseTooLarge))?;
    }
    io::stdout().lock().write_all(&bytes.0)
}
fn git_error(error: git2::Error) -> GitReason {
    match error.code() {
        ErrorCode::NotFound => GitReason::NotRepository,
        ErrorCode::UnbornBranch => GitReason::UnbornHead,
        ErrorCode::Locked | ErrorCode::Modified => GitReason::GitChanged,
        _ if error.class() == git2::ErrorClass::Os => GitReason::Unreadable,
        _ => GitReason::InvalidRepository,
    }
}
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadSummary {
    head: GitHead,
    upstream: Option<GitUpstream>,
}
impl HeadSummary {
    fn with_working_tree(self, working_tree: GitWorkingTree) -> GitSummary {
        GitSummary::Available {
            head: self.head,
            upstream: self.upstream,
            working_tree,
        }
    }
}
fn head_summary(repo: &Repository) -> Result<HeadSummary, GitReason> {
    let head = repo.head().map_err(git_error)?;
    let oid = head.target().ok_or(GitReason::InvalidRepository)?;
    repo.find_commit(oid)
        .map_err(|_| GitReason::InvalidRepository)?;
    let commit = oid.to_string();
    let detached = repo.head_detached().map_err(git_error)?;
    let name = if detached {
        None
    } else {
        Some(
            head.shorthand()
                .map_err(|_| GitReason::UnsupportedPath)?
                .to_owned(),
        )
    };
    let upstream = if let Some(name) = &name {
        let config = repo.config().map_err(git_error)?;
        let remote = config.get_string(&format!("branch.{name}.remote"));
        let merge = config.get_string(&format!("branch.{name}.merge"));
        if remote.is_err() && merge.is_err() {
            if remote
                .as_ref()
                .is_err_and(|error| error.code() != ErrorCode::NotFound)
                || merge
                    .as_ref()
                    .is_err_and(|error| error.code() != ErrorCode::NotFound)
            {
                return Err(GitReason::TrackingUnavailable);
            }
            None
        } else {
            let branch = repo
                .find_branch(name, BranchType::Local)
                .map_err(|_| GitReason::TrackingUnavailable)?;
            let upstream = branch
                .upstream()
                .map_err(|_| GitReason::TrackingUnavailable)?;
            let upstream_oid = upstream
                .get()
                .target()
                .ok_or(GitReason::TrackingUnavailable)?;
            repo.find_commit(upstream_oid)
                .map_err(|_| GitReason::TrackingUnavailable)?;
            let (ahead, behind) = repo
                .graph_ahead_behind(oid, upstream_oid)
                .map_err(|_| GitReason::TrackingUnavailable)?;
            Some(GitUpstream {
                name: upstream
                    .name()
                    .map_err(|_| GitReason::TrackingUnavailable)?
                    .ok_or(GitReason::UnsupportedPath)?
                    .to_owned(),
                ahead,
                behind,
                basis: "last_fetch".into(),
            })
        }
    } else {
        None
    };
    Ok(HeadSummary {
        head: GitHead {
            kind: if detached {
                GitHeadKind::Detached
            } else {
                GitHeadKind::Branch
            },
            name,
            short_commit: commit[..7].to_owned(),
            commit,
        },
        upstream,
    })
}
fn measure(root: &Path, mode: &str, requested: Option<&str>) -> Result<WorkerResponse, GitReason> {
    let repo = Repository::open_ext(root, git2::RepositoryOpenFlags::NO_SEARCH, &[] as &[&Path])
        .map_err(git_error)?;
    if repo.is_bare() || repo.workdir().is_none() {
        return Err(GitReason::InvalidRepository);
    }
    let selected_root = root.canonicalize().map_err(|_| GitReason::Unreadable)?;
    let workdir = repo
        .workdir()
        .ok_or(GitReason::InvalidRepository)?
        .canonicalize()
        .map_err(|_| GitReason::Unreadable)?;
    if workdir != selected_root {
        return Err(GitReason::InvalidRepository);
    }
    let head = head_summary(&repo)?;
    if mode == "head" {
        if head_summary(&repo)? != head {
            return Err(GitReason::GitChanged);
        }
        return Ok(WorkerResponse::Head(Ok(head)));
    }
    let response = match measure_changes(&repo, &head, mode, requested) {
        Ok(response) => response,
        Err(reason) if mode == "summary" => WorkerResponse::Summary(
            head.clone()
                .with_working_tree(GitWorkingTree::Unavailable { reason }),
        ),
        Err(reason) => return Err(reason),
    };
    if head_summary(&repo)? != head {
        return Err(GitReason::GitChanged);
    }
    Ok(response)
}
fn measure_changes(
    repo: &Repository,
    head: &HeadSummary,
    mode: &str,
    requested: Option<&str>,
) -> Result<WorkerResponse, GitReason> {
    let index = repo.index().map_err(git_error)?;
    if index.has_conflicts() {
        return Err(GitReason::Conflicted);
    }
    let index_stamp = index
        .path()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| (meta.len(), meta.modified().ok()));
    let oid = git2::Oid::from_str(&head.head.commit).map_err(git_error)?;
    let tree = repo
        .find_commit(oid)
        .map_err(git_error)?
        .tree()
        .map_err(git_error)?;
    let mut options = DiffOptions::new();
    // No UPDATE_INDEX: libgit2 documents that the index is left untouched.
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_typechange(true)
        .include_unreadable(true)
        .update_index(false)
        .ignore_submodules(false);
    if mode == "diff" {
        options.show_untracked_content(true);
    }
    // Compare HEAD directly with current files. The index is metadata, not an
    // intermediate side: staged deletion/recreation must measure current content.
    let mut diff = repo
        .diff_tree_to_workdir(Some(&tree), Some(&mut options))
        .map_err(git_error)?;
    // A new index-tracked file remains tracked even if an ignore rule now
    // matches it. Direct workdir comparison labels that file ignored, so add
    // only those HEAD-absent paths through an exact, pruned index-aware read.
    // HEAD-present paths never pass through Git's staged-deletion merge policy.
    let mut tracked_ignored = Vec::new();
    for entry in index.iter() {
        let path = std::str::from_utf8(&entry.path).map_err(|_| GitReason::UnsupportedPath)?;
        validate_path(path).map_err(|_| GitReason::UnsupportedPath)?;
        match tree.get_path(Path::new(path)) {
            Ok(_) => continue,
            Err(error) if error.code() == ErrorCode::NotFound => {}
            Err(error) => return Err(git_error(error)),
        }
        if repo
            .status_should_ignore(Path::new(path))
            .map_err(git_error)?
        {
            tracked_ignored.push(path.to_owned());
        }
    }
    if !tracked_ignored.is_empty() {
        // Keep unchanged index->workdir deltas so libgit2's internal merge
        // retains WORKDIR as the new source, even for index-identical additions.
        // The primary diff still drops all unmodified entries when merging.
        options
            .disable_pathspec_match(true)
            .include_untracked(false)
            .include_unmodified(true);
        for path in &tracked_ignored {
            options.pathspec(path);
        }
        let supplemental = repo
            .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut options))
            .map_err(git_error)?;
        diff.merge(&supplemental).map_err(git_error)?;
    }
    let mut find = DiffFindOptions::new();
    find.renames(true).for_untracked(true);
    diff.find_similar(Some(&mut find)).map_err(git_error)?;
    let mut changes = Vec::new();
    let mut selected = None;
    for (index, delta) in diff.deltas().enumerate() {
        let Some(change) = change(&delta)? else {
            continue;
        };
        if requested.is_some_and(|path| {
            path == change.path || change.previous_path.as_deref() == Some(path)
        }) {
            selected = Some((index, change.clone()));
        }
        changes.push(change);
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    let git = head.clone().with_working_tree(GitWorkingTree::Available {
        changed_files: changes.len(),
    });
    let response = if mode == "summary" {
        WorkerResponse::Summary(git)
    } else if mode == "diff" {
        let result = match selected {
            Some((index, change)) => Ok(GitDiff {
                git: git.clone(),
                file: Some(file_diff(&diff, index, change)?),
            }),
            None => Err(GitRequestError::NotFound),
        };
        WorkerResponse::Diff(result)
    } else {
        WorkerResponse::Status(GitStatus {
            git: git.clone(),
            changes: Some(changes),
        })
    };
    let after_stamp = index
        .path()
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| (meta.len(), meta.modified().ok()));
    if after_stamp != index_stamp {
        return Err(GitReason::GitChanged);
    }
    Ok(response)
}
fn path_string(path: Option<&Path>) -> Result<String, GitReason> {
    let value = path
        .and_then(Path::to_str)
        .ok_or(GitReason::UnsupportedPath)?;
    validate_path(value).map_err(|_| GitReason::UnsupportedPath)?;
    Ok(value.to_owned())
}
fn change(delta: &git2::DiffDelta<'_>) -> Result<Option<GitChange>, GitReason> {
    let status = match delta.status() {
        Delta::Unmodified | Delta::Ignored => return Ok(None),
        Delta::Added | Delta::Untracked | Delta::Copied => GitChangeKind::Added,
        Delta::Deleted => GitChangeKind::Deleted,
        Delta::Renamed => GitChangeKind::Renamed,
        Delta::Conflicted => return Err(GitReason::Conflicted),
        Delta::Unreadable => return Err(GitReason::Unreadable),
        _ => GitChangeKind::Modified,
    };
    let path = if status == GitChangeKind::Deleted {
        path_string(delta.old_file().path())?
    } else {
        path_string(delta.new_file().path())?
    };
    let previous_path = if status == GitChangeKind::Renamed {
        Some(path_string(delta.old_file().path())?)
    } else {
        None
    };
    Ok(Some(GitChange {
        path,
        previous_path,
        status,
    }))
}
fn text(bytes: &[u8]) -> Result<String, GitReason> {
    String::from_utf8(bytes.to_vec()).map_err(|_| GitReason::UnsupportedPath)
}
fn file_diff(
    diff: &git2::Diff<'_>,
    index: usize,
    change: GitChange,
) -> Result<GitFileDiff, GitReason> {
    let patch = git2::Patch::from_diff(diff, index).map_err(git_error)?;
    let delta = diff.get_delta(index).ok_or(GitReason::GitChanged)?;
    let binary = delta.flags().contains(git2::DiffFlags::BINARY)
        || delta.old_file().mode() == git2::FileMode::Commit
        || delta.new_file().mode() == git2::FileMode::Commit;
    let mut hunks = Vec::new();
    let mut bytes = 0usize;
    if let Some(patch) = patch.filter(|_| !binary) {
        for hunk_index in 0..patch.num_hunks() {
            let (hunk, count) = patch.hunk(hunk_index).map_err(git_error)?;
            let mut lines = Vec::new();
            for line_index in 0..count {
                let line = patch
                    .line_in_hunk(hunk_index, line_index)
                    .map_err(git_error)?;
                let kind = match line.origin() {
                    ' ' => GitLineKind::Context,
                    '+' => GitLineKind::Added,
                    '-' => GitLineKind::Deleted,
                    _ => continue, // EOF markers are not source lines.
                };
                bytes = bytes
                    .saturating_add(line.content().len())
                    .saturating_add(32);
                if bytes > RESPONSE_LIMIT {
                    return Err(GitReason::ResponseTooLarge);
                }
                lines.push(GitLine {
                    kind,
                    text: text(line.content())?,
                });
            }
            hunks.push(GitHunk {
                old_start: hunk.old_start(),
                old_lines: hunk.old_lines(),
                new_start: hunk.new_start(),
                new_lines: hunk.new_lines(),
                header: text(hunk.header())?,
                lines,
            });
        }
    }
    Ok(GitFileDiff {
        path: change.path,
        previous_path: change.previous_path,
        status: change.status,
        binary,
        hunks,
    })
}
