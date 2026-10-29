use git2::{Oid, Repository, RepositoryInitOptions, Signature};
use std::{
    fs,
    path::{Path, PathBuf},
};
use trellis::git_status::{
    GitChangeKind, GitHeadKind, GitReader, GitReason, GitRequestError, GitSummary, READ_DEADLINE,
    RESPONSE_LIMIT,
};

struct Fixture {
    temp: tempfile::TempDir,
    repo: Repository,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut options = RepositoryInitOptions::new();
        options.initial_head("main");
        let repo = Repository::init_opts(temp.path(), &options).unwrap();
        let this = Self { temp, repo };
        this.write("tracked.txt", "original\n");
        this.stage("tracked.txt");
        this.commit("initial");
        this
    }
    fn root(&self) -> &Path {
        self.temp.path()
    }
    fn write(&self, path: &str, content: &str) {
        fs::write(self.root().join(path), content).unwrap();
    }
    fn stage(&self, path: &str) {
        let mut index = self.repo.index().unwrap();
        index.add_path(Path::new(path)).unwrap();
        index.write().unwrap();
    }
    fn commit(&self, message: &str) -> Oid {
        let mut index = self.repo.index().unwrap();
        let tree = self.repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = Signature::now("Git reader test", "reader@example.invalid").unwrap();
        let parent = self
            .repo
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents: Vec<_> = parent.iter().collect();
        self.repo
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                message,
                &tree,
                &parents,
            )
            .unwrap()
    }
}
fn reader() -> GitReader {
    GitReader::new(PathBuf::from(env!("CARGO_BIN_EXE_trellis")))
}
fn available(
    summary: GitSummary,
) -> (
    trellis::git_status::GitHead,
    Option<trellis::git_status::GitUpstream>,
) {
    match summary {
        GitSummary::Available { head, upstream, .. } => (head, upstream),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn branch_tracking_uses_local_remote_ref_and_detects_missing_tracking() {
    let f = Fixture::new();
    let base = f.repo.head().unwrap().target().unwrap();
    f.repo
        .reference("refs/remotes/origin/main", base, true, "test")
        .unwrap();
    let mut config = f.repo.config().unwrap();
    config
        .set_str("remote.origin.url", "https://example.invalid/never-contact")
        .unwrap();
    config
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    f.write("tracked.txt", "local commit\n");
    f.stage("tracked.txt");
    f.commit("ahead");
    let (head, upstream) = available(reader().summary(f.root()).await);
    assert_eq!(head.kind, GitHeadKind::Branch);
    assert_eq!(head.name.as_deref(), Some("main"));
    let upstream = upstream.unwrap();
    assert_eq!((upstream.ahead, upstream.behind), (1, 0));
    assert_eq!(upstream.name, "origin/main");
    assert_eq!(upstream.basis, "last_fetch");
    // Move only the measured remote-tracking ref to a divergent commit, without a fetch.
    let signature = Signature::now("test", "test@example.invalid").unwrap();
    let parent = f.repo.find_commit(base).unwrap();
    let tree = parent.tree().unwrap();
    let remote = f
        .repo
        .commit(None, &signature, &signature, "remote", &tree, &[&parent])
        .unwrap();
    f.repo
        .reference("refs/remotes/origin/main", remote, true, "test")
        .unwrap();
    let (_, upstream) = available(reader().summary(f.root()).await);
    let upstream = upstream.unwrap();
    assert_eq!((upstream.ahead, upstream.behind), (1, 1));
    f.repo
        .find_reference("refs/remotes/origin/main")
        .unwrap()
        .delete()
        .unwrap();
    assert_eq!(
        reader().summary(f.root()).await,
        GitSummary::Unavailable {
            reason: GitReason::TrackingUnavailable
        }
    );
}

#[tokio::test]
async fn detached_no_upstream_non_repository_and_unborn_are_explicit() {
    let f = Fixture::new();
    let (head, upstream) = available(reader().summary(f.root()).await);
    assert_eq!(head.name.as_deref(), Some("main"));
    assert!(upstream.is_none());
    let oid = f.repo.head().unwrap().target().unwrap();
    f.repo.set_head_detached(oid).unwrap();
    let (head, upstream) = available(reader().summary(f.root()).await);
    assert_eq!(head.kind, GitHeadKind::Detached);
    assert_eq!(head.name, None);
    assert_eq!(head.commit, oid.to_string());
    assert_eq!(head.short_commit, oid.to_string()[..7]);
    assert!(upstream.is_none());
    let plain = tempfile::tempdir().unwrap();
    assert_eq!(
        reader().summary(plain.path()).await,
        GitSummary::Unavailable {
            reason: GitReason::NotRepository
        }
    );
    let result = reader().status(plain.path()).await;
    assert!(result.changes.is_none());
    Repository::init(plain.path()).unwrap();
    assert_eq!(
        reader().summary(plain.path()).await,
        GitSummary::Unavailable {
            reason: GitReason::UnbornHead
        }
    );
}

#[tokio::test]
async fn linked_worktree_reads_its_own_head() {
    let f = Fixture::new();
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("linked");
    f.repo.worktree("linked", &path, None).unwrap();
    let (head, _) = available(reader().summary(&path).await);
    assert_eq!(head.name.as_deref(), Some("linked"));
    fs::write(path.join("tracked.txt"), "linked modification\n").unwrap();
    let status = reader().status(&path).await;
    assert_eq!(status.changes.unwrap()[0].path, "tracked.txt");
    assert!(reader().status(f.root()).await.changes.unwrap().is_empty());
}

#[tokio::test]
async fn status_and_diff_measure_net_head_with_rename_delete_added_and_ignored() {
    let f = Fixture::new();
    f.write("deleted.txt", "delete me\n");
    f.stage("deleted.txt");
    f.write("rename-old.txt", "rename me without editing\n");
    f.stage("rename-old.txt");
    f.write(".gitignore", "ignored.txt\n");
    f.stage(".gitignore");
    f.commit("baseline");
    f.write("tracked.txt", "staged\n");
    f.stage("tracked.txt");
    f.write("tracked.txt", "final without newline");
    fs::rename(
        f.root().join("rename-old.txt"),
        f.root().join("rename-new.txt"),
    )
    .unwrap();
    fs::remove_file(f.root().join("deleted.txt")).unwrap();
    f.write("untracked.txt", "new\n");
    f.write("ignored.txt", "hidden\n");
    f.write("added.txt", "staged addition\n");
    f.stage("added.txt");
    let status = reader().status(f.root()).await;
    let summary = serde_json::to_value(reader().summary(f.root()).await).unwrap();
    let expected = serde_json::json!({"state": "available", "changedFiles": 5});
    assert_eq!(summary["workingTree"], expected);
    assert_eq!(
        serde_json::to_value(&status.git).unwrap()["workingTree"],
        expected
    );
    let changes = status.changes.unwrap();
    assert_eq!(changes.len(), 5, "{changes:?}");
    assert!(changes.iter().any(|c| c.path == "rename-new.txt"
        && c.previous_path.as_deref() == Some("rename-old.txt")
        && c.status == GitChangeKind::Renamed));
    assert!(
        changes
            .iter()
            .any(|c| c.path == "deleted.txt" && c.status == GitChangeKind::Deleted)
    );
    assert!(
        changes
            .iter()
            .any(|c| c.path == "untracked.txt" && c.status == GitChangeKind::Added)
    );
    let file = reader()
        .diff(f.root(), "tracked.txt")
        .await
        .unwrap()
        .file
        .unwrap();
    assert!(!file.binary);
    let texts: Vec<_> = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|l| l.text.as_str()))
        .collect();
    assert!(texts.contains(&"original\n"));
    assert!(texts.contains(&"final without newline"));
    assert!(!texts.contains(&"staged\n"));
    let deleted = reader()
        .diff(f.root(), "deleted.txt")
        .await
        .unwrap()
        .file
        .unwrap();
    assert_eq!(deleted.status, GitChangeKind::Deleted);
    assert_eq!(
        reader()
            .diff(f.root(), "rename-old.txt")
            .await
            .unwrap()
            .file
            .unwrap()
            .path,
        "rename-new.txt"
    );
    assert_eq!(
        reader().diff(f.root(), "not-changed").await,
        Err(GitRequestError::NotFound)
    );
    assert_eq!(
        reader().diff(f.root(), "../tracked.txt").await,
        Err(GitRequestError::InvalidPath)
    );
}

#[tokio::test]
async fn staged_edit_undone_in_worktree_is_clean_and_binary_is_not_suppressed() {
    let f = Fixture::new();
    f.write("tracked.txt", "staged change\n");
    f.stage("tracked.txt");
    f.write("tracked.txt", "original\n");
    assert!(reader().status(f.root()).await.changes.unwrap().is_empty());
    fs::write(f.root().join("binary.bin"), b"before\0binary").unwrap();
    f.stage("binary.bin");
    f.commit("binary");
    fs::write(f.root().join("binary.bin"), b"after\0binary").unwrap();
    let result = reader()
        .diff(f.root(), "binary.bin")
        .await
        .unwrap()
        .file
        .unwrap();
    assert!(result.binary);
    assert!(result.hunks.is_empty());
}

#[tokio::test]
async fn summary_count_reuses_complete_net_changes_and_successful_selected_counts() {
    let f = Fixture::new();
    let summary = serde_json::to_value(reader().summary(f.root()).await).unwrap();
    assert_eq!(
        summary["workingTree"],
        serde_json::json!({"state": "available", "changedFiles": 0})
    );
    f.write("deleted.txt", "delete me\n");
    f.stage("deleted.txt");
    f.write("rename-old.txt", "rename me without editing\n");
    f.stage("rename-old.txt");
    f.write(".gitignore", "ignored.txt\n");
    f.stage(".gitignore");
    f.commit("baseline");
    f.write("tracked.txt", "staged\n");
    f.stage("tracked.txt");
    f.write("tracked.txt", "original\n"); // Staged-only edit undone: not changed.
    fs::rename(
        f.root().join("rename-old.txt"),
        f.root().join("rename-new.txt"),
    )
    .unwrap();
    fs::remove_file(f.root().join("deleted.txt")).unwrap();
    f.write("untracked.txt", "new\n");
    f.write("ignored.txt", "hidden\n");
    let status = reader().status(f.root()).await;
    let changes = status.changes.unwrap();
    assert_eq!(changes.len(), 3, "{changes:?}");
    let expected = serde_json::json!({"state": "available", "changedFiles": 3});
    let summary = serde_json::to_value(reader().summary(f.root()).await).unwrap();
    assert_eq!(summary["workingTree"], expected);
    assert!(summary.get("changes").is_none());
    assert_eq!(
        serde_json::to_value(status.git).unwrap()["workingTree"],
        expected
    );
    let diff = reader().diff(f.root(), "rename-new.txt").await.unwrap();
    assert_eq!(
        serde_json::to_value(diff.git).unwrap()["workingTree"],
        expected
    );
}

#[tokio::test]
async fn conflicted_index_is_unavailable_never_clean() {
    let f = Fixture::new();
    let head = f.repo.head().unwrap().target().unwrap();
    f.repo
        .reference("refs/remotes/origin/main", head, true, "test")
        .unwrap();
    let mut config = f.repo.config().unwrap();
    config
        .set_str("remote.origin.url", "https://example.invalid/never-contact")
        .unwrap();
    config
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    let mut index = f.repo.index().unwrap();
    let mut entry = index.get_path(Path::new("tracked.txt"), 0).unwrap();
    index.remove_path(Path::new("tracked.txt")).unwrap();
    entry.flags = (entry.flags & !0x3000) | 0x1000;
    index.add(&entry).unwrap();
    entry.flags = (entry.flags & !0x3000) | 0x2000;
    index.add(&entry).unwrap();
    index.write().unwrap();
    assert!(index.has_conflicts());
    let summary = serde_json::to_value(reader().summary(f.root()).await).unwrap();
    assert_eq!(summary["state"], "available");
    assert_eq!(summary["head"]["name"], "main");
    assert_eq!(
        summary["upstream"],
        serde_json::json!({
            "name": "origin/main", "ahead": 0, "behind": 0, "basis": "last_fetch"
        })
    );
    assert_eq!(
        summary["workingTree"],
        serde_json::json!({"state": "unavailable", "reason": "conflicted"})
    );
    let status = reader().status(f.root()).await;
    assert_eq!(
        status.git,
        GitSummary::Unavailable {
            reason: GitReason::Conflicted
        }
    );
    assert!(status.changes.is_none());
}

#[tokio::test]
async fn large_diff_returns_typed_complete_failure() {
    let f = Fixture::new();
    f.write(
        "large.txt",
        &"content line with enough bytes\n".repeat(RESPONSE_LIMIT / 20),
    );
    let result = reader().diff(f.root(), "large.txt").await.unwrap();
    assert_eq!(
        result.git,
        GitSummary::Unavailable {
            reason: GitReason::ResponseTooLarge
        }
    );
    assert!(result.file.is_none());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_trellis"))
        .args(["git-read", "--mode", "diff", "--root"])
        .arg(f.root())
        .args(["--path", "large.txt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.len() <= RESPONSE_LIMIT);
    let _: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
}

#[cfg(unix)]
fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
#[cfg(unix)]
#[tokio::test]
async fn configured_programs_do_not_run_and_index_bytes_and_stat_do_not_change() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let programs = tempfile::tempdir().unwrap();
    let sentinel = programs.path().join("executed");
    let helper = programs.path().join("helper");
    script(
        &helper,
        &format!("#!/bin/sh\nprintf called >> '{}'\n", sentinel.display()),
    );
    let hooks = programs.path().join("hooks");
    fs::create_dir(&hooks).unwrap();
    for hook in [
        "post-index-change",
        "pre-commit",
        "post-checkout",
        "fsmonitor-watchman",
    ] {
        script(
            &hooks.join(hook),
            &format!("#!/bin/sh\nprintf called >> '{}'\n", sentinel.display()),
        );
    }
    // Set dangerous config only after creating the baseline fixture.
    let mut config = f.repo.config().unwrap();
    config
        .set_str("core.hooksPath", hooks.to_str().unwrap())
        .unwrap();
    config
        .set_str("core.fsmonitor", helper.to_str().unwrap())
        .unwrap();
    config
        .set_str("credential.helper", &format!("!{}", helper.display()))
        .unwrap();
    for key in [
        "filter.probe.clean",
        "filter.probe.smudge",
        "filter.probe.process",
        "diff.probe.command",
        "diff.probe.textconv",
        "diff.external",
    ] {
        config.set_str(key, helper.to_str().unwrap()).unwrap();
    }
    f.write(".gitattributes", "tracked.txt filter=probe diff=probe\n");
    f.write("tracked.txt", "modified\n");
    let index_path = f.repo.path().join("index");
    let before_bytes = fs::read(&index_path).unwrap();
    let before = fs::metadata(&index_path).unwrap();
    available(reader().summary(f.root()).await);
    let status = reader().status(f.root()).await;
    assert!(status.changes.is_some());
    let diff = reader().diff(f.root(), "tracked.txt").await.unwrap();
    assert!(diff.file.is_some());
    assert!(!sentinel.exists(), "configured repository program executed");
    assert_eq!(fs::read(&index_path).unwrap(), before_bytes);
    let after = fs::metadata(&index_path).unwrap();
    assert_eq!(
        (
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
            before.ctime(),
            before.ctime_nsec()
        ),
        (
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        )
    );
}

#[cfg(unix)]
#[tokio::test]
async fn deadline_kills_and_reaps_child_and_cancellation_also_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("slow");
    let pid_file = dir.path().join("pid");
    // exec keeps the PID: the child blocks without burning CPU or leaving descendants.
    script(
        &exe,
        &format!(
            "#!/bin/sh\nprintf '%s' $$ > '{}'\nexec /bin/sleep 60\n",
            pid_file.display()
        ),
    );
    let start = std::time::Instant::now();
    let result = GitReader::new(exe.clone()).summary(dir.path()).await;
    assert_eq!(
        result,
        GitSummary::Unavailable {
            reason: GitReason::DeadlineExceeded
        }
    );
    assert!(start.elapsed() >= READ_DEADLINE);
    assert!(start.elapsed() < READ_DEADLINE + std::time::Duration::from_secs(2));
    let pid: i32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    assert_reaped(pid).await;
    fs::remove_file(&pid_file).unwrap();
    let root = dir.path().to_owned();
    let task = tokio::spawn(async move { GitReader::new(exe).summary(&root).await });
    let pid = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(text) = fs::read_to_string(&pid_file)
                && let Ok(pid) = text.parse::<i32>()
            {
                break pid;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_reaped(pid).await;
}
#[cfg(unix)]
async fn assert_reaped(pid: i32) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            // ESRCH proves the PID is gone, not merely that a kill was sent.
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn staged_deletion_and_recreation_compares_current_content_with_head() {
    let f = Fixture::new();
    let mut index = f.repo.index().unwrap();
    index.remove_path(Path::new("tracked.txt")).unwrap();
    index.write().unwrap();
    // HEAD=A, index absent, worktree=A: staging does not invent a net change.
    assert!(reader().status(f.root()).await.changes.unwrap().is_empty());
    assert_eq!(
        reader().diff(f.root(), "tracked.txt").await,
        Err(GitRequestError::NotFound)
    );
    // HEAD=A, index absent, worktree=B: this is Modified, never Deleted.
    f.write("tracked.txt", "recreated\n");
    let changes = reader().status(f.root()).await.changes.unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].status, GitChangeKind::Modified);
    let file = reader()
        .diff(f.root(), "tracked.txt")
        .await
        .unwrap()
        .file
        .unwrap();
    assert_eq!(file.status, GitChangeKind::Modified);
    let lines: Vec<_> = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|line| line.text.as_str()))
        .collect();
    assert!(lines.contains(&"original\n"));
    assert!(lines.contains(&"recreated\n"));
}

#[tokio::test]
async fn staged_addition_removed_from_worktree_is_not_a_net_change() {
    let f = Fixture::new();
    f.write("added.txt", "staged but absent\n");
    f.stage("added.txt");
    fs::remove_file(f.root().join("added.txt")).unwrap();
    assert!(reader().status(f.root()).await.changes.unwrap().is_empty());
    assert_eq!(
        reader().diff(f.root(), "added.txt").await,
        Err(GitRequestError::NotFound)
    );
}

#[tokio::test]
async fn ignored_index_tracked_addition_is_measured_and_can_be_a_rename_target() {
    let f = Fixture::new();
    let path = "ignored/deep/[literal].txt";
    fs::create_dir_all(f.root().join("ignored/deep")).unwrap();
    f.write(path, "original\n");
    f.stage(path);
    f.write(".gitignore", "ignored/\n");
    f.stage(".gitignore");
    // Do not commit the added ignored file: HEAD deliberately has no entry.
    f.write(path, "current ignored tracked content\n");
    let changes = reader().status(f.root()).await.changes.unwrap();
    assert!(
        changes
            .iter()
            .any(|change| change.path == path && change.status == GitChangeKind::Added)
    );
    let file = reader().diff(f.root(), path).await.unwrap().file.unwrap();
    assert_eq!(file.status, GitChangeKind::Added);
    assert!(
        file.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|line| line.text == "current ignored tracked content\n")
    );
    // The supplemental Added delta must be eligible for rename detection.
    f.write(path, "original\n");
    fs::remove_file(f.root().join("tracked.txt")).unwrap();
    let rename_status = reader().status(f.root()).await;
    let result = reader().diff(f.root(), "tracked.txt").await.unwrap();
    assert!(
        result.file.is_some(),
        "rename result: {result:?}; status: {rename_status:?}"
    );
    let file = result.file.unwrap();
    assert_eq!(file.status, GitChangeKind::Renamed);
    assert_eq!(file.path, path);
    assert_eq!(file.previous_path.as_deref(), Some("tracked.txt"));
    fs::remove_file(f.root().join(path)).unwrap();
    assert!(
        !reader()
            .status(f.root())
            .await
            .changes
            .unwrap()
            .iter()
            .any(|change| change.path == path)
    );
}

#[tokio::test]
async fn repository_workdir_cannot_redirect_selected_root() {
    let f = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    fs::write(
        other.path().join("tracked.txt"),
        "outside selected checkout\n",
    )
    .unwrap();
    f.repo
        .config()
        .unwrap()
        .set_str("core.worktree", other.path().to_str().unwrap())
        .unwrap();
    let expected = GitSummary::Unavailable {
        reason: GitReason::InvalidRepository,
    };
    assert_eq!(reader().summary(f.root()).await, expected);
    let status = reader().status(f.root()).await;
    assert_eq!(status.git, expected);
    assert!(status.changes.is_none());
    let diff = reader().diff(f.root(), "tracked.txt").await.unwrap();
    assert_eq!(diff.git, expected);
    assert!(diff.file.is_none());
}

#[tokio::test]
async fn missing_or_noncommit_head_never_reports_available() {
    let f = Fixture::new();
    let missing = "1111111111111111111111111111111111111111";
    fs::write(
        f.repo.path().join("refs/heads/main"),
        format!("{missing}\n"),
    )
    .unwrap();
    assert_eq!(
        reader().summary(f.root()).await,
        GitSummary::Unavailable {
            reason: GitReason::InvalidRepository
        }
    );
    // Equal missing HEAD/tracking OIDs must not pass graph's equality shortcut.
    fs::create_dir_all(f.repo.path().join("refs/remotes/origin")).unwrap();
    fs::write(
        f.repo.path().join("refs/remotes/origin/main"),
        format!("{missing}\n"),
    )
    .unwrap();
    let mut config = f.repo.config().unwrap();
    config
        .set_str("remote.origin.url", "https://example.invalid/never-contact")
        .unwrap();
    config
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    assert_eq!(
        reader().summary(f.root()).await,
        GitSummary::Unavailable {
            reason: GitReason::InvalidRepository
        }
    );
    let blob = f.repo.blob(b"not a commit").unwrap();
    fs::write(f.repo.path().join("refs/heads/main"), format!("{blob}\n")).unwrap();
    assert_eq!(
        reader().summary(f.root()).await,
        GitSummary::Unavailable {
            reason: GitReason::InvalidRepository
        }
    );
}

#[tokio::test]
async fn missing_or_noncommit_tracking_object_never_reports_counts() {
    let f = Fixture::new();
    let valid = f.repo.head().unwrap().target().unwrap();
    f.repo
        .reference("refs/remotes/origin/main", valid, true, "fixture")
        .unwrap();
    let mut config = f.repo.config().unwrap();
    config
        .set_str("remote.origin.url", "https://example.invalid/never-contact")
        .unwrap();
    config
        .set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
        .unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    let ref_path = f.repo.path().join("refs/remotes/origin/main");
    fs::write(&ref_path, "1111111111111111111111111111111111111111\n").unwrap();
    let expected = GitSummary::Unavailable {
        reason: GitReason::TrackingUnavailable,
    };
    assert_eq!(reader().summary(f.root()).await, expected);
    let blob = f.repo.blob(b"not a commit").unwrap();
    fs::write(&ref_path, format!("{blob}\n")).unwrap();
    assert_eq!(reader().summary(f.root()).await, expected);
}
