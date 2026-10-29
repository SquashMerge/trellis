//! The trust-profile table through the real spawn path: every row's child
//! environment is a closed allow-list, and restricted scip-python's child can
//! resolve no interpreter. The stub stands in for each row's launcher.
#![cfg(feature = "test-support")]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use trellis::producer::{
    ProducerName, TrustProfile,
    detect::{Declared, Environment, Exe, Resolution, Verification},
    exec::{Hooks, Limits, ProducerSpec, spawn_producer},
    profiles::{self, Stub, Target},
    worktree::write_bin_links,
};

#[path = "support/producer.rs"]
mod support;
use support::{stub_path, write_mode};

const CANARIES: [&str; 10] = [
    "NODE_OPTIONS",
    "JAVA_TOOL_OPTIONS",
    "RUSTC_WRAPPER",
    "PYTHONPATH",
    "LD_PRELOAD",
    "DYLD_INSERT_LIBRARIES",
    "AWS_SECRET_ACCESS_KEY",
    "GITHUB_TOKEN",
    "HTTPS_PROXY",
    "TRELLIS_CANARY",
];

fn executable(path: &Path) {
    fs::write(path, "#!/bin/sh\n").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn exe(path: &Path, dir: &Path) -> Exe {
    Exe {
        path: path.to_owned(),
        sha256: "0".repeat(64),
        dir: dir.to_owned(),
    }
}

/// A declared environment whose `decl` directory holds every interpreter and
/// runtime a profile could expose.
struct World {
    dir: tempfile::TempDir,
    declared: Declared,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let decl = dir.path().join("decl");
        fs::create_dir(&decl).unwrap();
        for name in ["python3", "python", "node", "java", "rustup", "scip-java"] {
            executable(&decl.join(name));
        }
        let declared = Declared::new(&Environment {
            path: format!("{}:/usr/bin:/bin", decl.display()),
            home: dir.path().join("home").display().to_string(),
            ..Environment::default()
        })
        .unwrap();
        Self { dir, declared }
    }

    fn decl(&self) -> PathBuf {
        self.dir.path().join("decl")
    }

    /// A resolution whose launcher is `launcher` and whose `node` is the stub.
    fn resolution(&self, launcher: &Path) -> Resolution {
        let decl = self.decl();
        Resolution {
            launcher: launcher.to_owned(),
            verification: Verification {
                method: "testStub",
                observed: String::new(),
            },
            node: Some(exe(&stub_path(), &decl)),
            java: Some(exe(&decl.join("java"), &decl)),
            java_home: None,
            rustup: Some(exe(&decl.join("rustup"), &decl)),
            python3: Some(exe(&decl.join("python3"), &decl)),
        }
    }

    fn run_dir(&self, name: &str) -> PathBuf {
        let run = self.dir.path().join(name);
        for sub in ["work", "logs", "out"] {
            fs::create_dir_all(run.join(sub)).unwrap();
        }
        run
    }
}

fn spawn(argv: Vec<String>, env: BTreeMap<String, String>, run: &Path) {
    let spec = ProducerSpec {
        argv: argv.into_iter().map(Into::into).collect(),
        cwd: run.join("work"),
        env,
        artifact: run.join("out/index.scip"),
        log_dir: Some(run.join("logs")),
    };
    let limits = Limits {
        timeout: std::time::Duration::from_secs(120),
        ..Limits::default()
    };
    let outcome = spawn_producer(
        &spec,
        &limits,
        &AtomicBool::new(false),
        &mut Hooks::default(),
    )
    .unwrap();
    assert_eq!(outcome.diagnostics.exit.code, Some(0), "{outcome:?}");
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn env_canaries_absent_per_profile_row() {
    // Each test runs in its own process under nextest, so this is not racy.
    for name in CANARIES {
        unsafe { std::env::set_var(name, "canary") };
    }
    let world = World::new();
    let stub = Stub {
        path: stub_path(),
        args: Vec::new(),
    };
    let rows = profiles::rows(true);
    assert_eq!(rows.len(), 9);
    for (n, row) in rows.iter().enumerate() {
        let run = world.run_dir(&format!("run-{n}"));
        let dump = run.join("env.json");
        let mode = write_mode(&run, "mode.json", &json!({"envdump": dump}));
        let pin = row.target.pin().unwrap();
        let resolution = world.resolution(&mode);
        let run_text = run.display().to_string();
        let spec = profiles::command(
            row,
            &resolution,
            &world.declared,
            Some(&stub),
            &pin,
            &run_text,
        );
        // The stub takes the row's place as argv[0], keeping every argument.
        let mut argv = vec![
            stub_path().display().to_string(),
            "--mode-file".to_owned(),
            mode.display().to_string(),
        ];
        argv.extend(spec.argv[1..].iter().cloned());
        spawn(argv, spec.env.clone(), &run);
        let report = read(&dump);
        let env: BTreeMap<String, String> = serde_json::from_value(report["env"].clone()).unwrap();
        assert_eq!(env, spec.env, "{} {:?}", row.target.name(), row.trust);
        for canary in CANARIES {
            assert!(!env.contains_key(canary), "{canary} leaked");
        }
        let path = &env["PATH"];
        assert!(!path.contains(&world.dir.path().join("home").display().to_string()));
    }
}

#[test]
fn restricted_python_child_cannot_resolve_interpreter() {
    let world = World::new();
    let run = world.run_dir("python");
    let dump = run.join("env.json");
    let mode = write_mode(
        &run,
        "mode.json",
        &json!({"envdump": dump, "which": "python3,python,node"}),
    );
    let row = profiles::row(
        Target::Real(ProducerName::ScipPython),
        TrustProfile::Restricted,
    )
    .unwrap();
    let pin = row.target.pin().unwrap();
    // The stub is `node` and the mode file is `main.js`.
    let resolution = world.resolution(&mode);
    let run_text = run.display().to_string();
    let spec = profiles::command(&row, &resolution, &world.declared, None, &pin, &run_text);
    assert_eq!(spec.env["PATH"], format!("{run_text}/bin"));
    write_bin_links(&run, &spec.bin_links).unwrap();
    assert_eq!(spec.argv[0], stub_path().display().to_string());
    spawn(spec.argv.clone(), spec.env.clone(), &run);
    let report = read(&dump);
    assert!(report["which"]["python3"].is_null());
    assert!(report["which"]["python"].is_null());
    assert!(!report["which"]["node"].is_null());
    assert_eq!(report["env"]["PATH"], format!("{run_text}/bin"));
    let bin: Vec<_> = fs::read_dir(run.join("bin"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(bin, ["node"]);
    assert!(
        spec.argv
            .contains(&format!("{run_text}/producer-config/environment.json")),
        "--environment supplies an empty package list"
    );
}
