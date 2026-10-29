//! Shared helpers for the producer test binaries. Later slices only add items:
//! binaries owned by earlier slices include this file and are not edited again.
#![allow(dead_code)]
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::{Builder, TempDir};

/// The stub producer built for this test run.
pub fn stub_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_trellis-producer-stub"))
}

/// Write the stub's mode file into `dir` and return its path.
pub fn write_mode(dir: &Path, name: &str, mode: &Value) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, serde_json::to_vec(mode).unwrap()).unwrap();
    path
}

/// The daemon environment that registers the stub as the test producer with
/// `mode`, written under `dir`.
pub fn stub_env(dir: &Path, mode: &Value) -> Vec<(String, String)> {
    let file = write_mode(dir, "mode.json", mode);
    vec![
        (
            "TRELLIS_TEST_PRODUCER_STUB".into(),
            stub_path().display().to_string(),
        ),
        (
            "TRELLIS_TEST_PRODUCER_STUB_ARGS".into(),
            json!(["--mode-file", file]).to_string(),
        ),
    ]
}

/// Executable `/bin/sh` scripts named like every producer launcher and runtime
/// that record their invocation in the returned marker file and exit 97.
pub fn tripwire_dir(tmp: &Path) -> (PathBuf, PathBuf) {
    let dir = tmp.join("tripwire");
    fs::create_dir_all(&dir).unwrap();
    let marker = tmp.join("tripwire.marker");
    for name in [
        "scip-java",
        "scip-typescript",
        "scip-python",
        "rust-analyzer",
        "rustup",
        "node",
        "java",
        "python3",
        "python",
    ] {
        let path = dir.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$0\" \"$*\" >> '{}'\nexit 97\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, marker)
}

/// SHA-256 over every entry's path, type, length, mtime and content hash or
/// link target, hidden entries and `.git` included.
pub fn workspace_tree_digest(root: &Path) -> String {
    fn walk(root: &Path, dir: &Path, hasher: &mut Sha256) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let meta = fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            let kind = meta.file_type();
            let detail = if kind.is_symlink() {
                format!("link {}", fs::read_link(&path).unwrap().display())
            } else if kind.is_dir() {
                "dir".to_owned()
            } else {
                format!(
                    "file {}",
                    hex::encode(Sha256::digest(fs::read(&path).unwrap()))
                )
            };
            hasher.update(format!(
                "{relative}\0{detail}\0{}\0{}{:09}\n",
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec()
            ));
            if kind.is_dir() {
                walk(root, &path, hasher);
            }
        }
    }
    let mut hasher = Sha256::new();
    walk(root, root, &mut hasher);
    hex::encode(hasher.finalize())
}

/// One checkout with a temporary `HOME`, indexed through the CLI (which starts
/// the daemon with `daemon_env`).
pub struct Checkout {
    pub temp: TempDir,
    pub home: PathBuf,
    pub root: PathBuf,
}

impl Checkout {
    pub fn cli(&self) -> Command {
        cli(&self.home)
    }
}

pub fn cli(home: &Path) -> Command {
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    command
}

/// Populate a fresh workspace with `setup`, then index it.
pub fn start_checkout(setup: impl FnOnce(&Path), daemon_env: &[(String, String)]) -> Checkout {
    let temp = Builder::new()
        .prefix("producer-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    fs::create_dir(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    setup(&root);
    let mut index = cli(&home);
    for (key, value) in daemon_env {
        index.env(key, value);
    }
    let output = index
        .args(["index", "--workspace", root.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Checkout { temp, home, root }
}

/// A fake npm global prefix under `dir` holding `package` (for example
/// `scip-typescript`) at `version`, with `bin/<package>` linked to its entry
/// point and a `bin/node` that has a native executable magic. Returns `bin/`.
pub fn fake_npm_install(dir: &Path, package: &str, version: &str) -> PathBuf {
    let bin = dir.join("bin");
    let root = dir.join("lib/node_modules/@sourcegraph").join(package);
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(root.join("dist")).unwrap();
    fs::write(
        root.join("package.json"),
        json!({"name": format!("@sourcegraph/{package}"), "version": version}).to_string(),
    )
    .unwrap();
    fs::write(root.join("dist/main.js"), "// entry\n").unwrap();
    std::os::unix::fs::symlink(root.join("dist/main.js"), bin.join(package)).unwrap();
    fs::set_permissions(root.join("dist/main.js"), fs::Permissions::from_mode(0o755)).unwrap();
    let node = bin.join("node");
    fs::write(&node, [0xcf, 0xfa, 0xed, 0xfe, 0, 0, 0, 0]).unwrap();
    fs::set_permissions(&node, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The pinned version of a producer, from the checked-in profile.
pub fn pinned_version(name: &str) -> String {
    trellis::scip_admission::pinned_producer_by_id(name)
        .unwrap()
        .version
}
