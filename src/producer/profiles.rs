//! The closed producer x trust-profile table (§3.3): the only source of a
//! producer command, its child environment and `PATH`, its worktree omissions
//! and additions, its policy reports and its generation provenance (D4).

use super::detect::{Declared, Resolution, RuntimeExecutable, rustup_toolchain};
use super::{PROFILE_UNSUPPORTED, ProducerName, TrustProfile};
use crate::scip_admission::PinnedProducer;
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// The pin id of the test-support stand-in producer.
pub const STUB_NAME: &str = crate::scip_admission::STUB_PRODUCER;
const RUST_REFUSAL: &str = "rust-analyzer cannot disable build scripts or proc macros";
const JAVA_BUILD_FILES: [&str; 6] = [
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
];
const SYSTEM_PATH: [&str; 2] = ["/usr/bin", "/bin"];

/// A producer the table has rows for: one of the four pinned producers, or
/// the test stub when the daemon registered one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Real(ProducerName),
    Stub,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Self::Real(producer) => producer.as_str(),
            Self::Stub => STUB_NAME,
        }
    }

    /// The languages its artifact covers.
    pub fn languages(self) -> &'static [&'static str] {
        match self {
            Self::Real(producer) => {
                crate::scip_admission::producer_languages(producer.as_str()).unwrap_or(&[])
            }
            Self::Stub => crate::scip_admission::stub_languages(),
        }
    }

    pub fn parse(name: &str, stub_registered: bool) -> Option<Self> {
        if name == STUB_NAME {
            return stub_registered.then_some(Self::Stub);
        }
        ProducerName::parse(name).map(Self::Real)
    }

    pub fn all(stub_registered: bool) -> Vec<Self> {
        ProducerName::ALL
            .into_iter()
            .map(Self::Real)
            .chain(stub_registered.then_some(Self::Stub))
            .collect()
    }

    pub fn pin(self) -> Option<PinnedProducer> {
        crate::scip_admission::pinned_producer_by_id(self.name())
    }
}

/// Names `--producer` accepts on the command line.
pub fn cli_producer_names() -> Vec<&'static str> {
    let mut names: Vec<_> = ProducerName::ALL.map(ProducerName::as_str).to_vec();
    if cfg!(feature = "test-support") {
        names.push(STUB_NAME);
    }
    names
}

/// The test producer, registered through the daemon's environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stub {
    pub path: PathBuf,
    pub args: Vec<String>,
}

impl Stub {
    /// `TRELLIS_TEST_PRODUCER_STUB` and `TRELLIS_TEST_PRODUCER_STUB_ARGS`
    /// (a JSON array); only a test-support build reads them.
    pub fn from_env() -> Option<Self> {
        #[cfg(feature = "test-support")]
        {
            let path = PathBuf::from(std::env::var_os("TRELLIS_TEST_PRODUCER_STUB")?);
            if !path.is_absolute() {
                return None;
            }
            let args = match std::env::var("TRELLIS_TEST_PRODUCER_STUB_ARGS") {
                Ok(raw) => serde_json::from_str(&raw).ok()?,
                Err(_) => Vec::new(),
            };
            Some(Self { path, args })
        }
        #[cfg(not(feature = "test-support"))]
        None
    }
}

/// One supported producer x trust-profile row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Row {
    pub target: Target,
    pub trust: TrustProfile,
}

/// The row for `target` under `trust`, or the typed refusal.
pub fn row(target: Target, trust: TrustProfile) -> Result<Row> {
    if target == Target::Real(ProducerName::RustAnalyzer) && trust == TrustProfile::Restricted {
        return Err(anyhow!("{PROFILE_UNSUPPORTED}: {RUST_REFUSAL}"));
    }
    Ok(Row { target, trust })
}

/// Every supported row.
pub fn rows(stub_registered: bool) -> Vec<Row> {
    Target::all(stub_registered)
        .into_iter()
        .flat_map(|target| {
            [TrustProfile::Restricted, TrustProfile::Trusted]
                .into_iter()
                .filter_map(move |trust| row(target, trust).ok())
        })
        .collect()
}

/// Why `row` refuses a trust profile, for status.
pub fn refusal(target: Target, trust: TrustProfile) -> Option<String> {
    row(target, trust).err().map(|_| RUST_REFUSAL.to_owned())
}

/// A symlink in the run's private `bin/`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinLink {
    pub name: String,
    pub target: PathBuf,
}

/// What the owner would run, with every path under `run`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub bin_links: Vec<BinLink>,
}

fn text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn path_with(dirs: &[&Path]) -> String {
    dirs.iter()
        .map(|dir| text(dir))
        .chain(SYSTEM_PATH.map(str::to_owned))
        .collect::<Vec<_>>()
        .join(":")
}

fn lang() -> &'static str {
    if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    }
}

/// The command of `row`. `resolution` must come from the same row (the
/// launcher, `node`, `java`, `rustup` and `python3` each row consumes), and `rules` from the
/// same plan.
pub fn command(
    row: &Row,
    resolution: &Resolution,
    declared: &Declared,
    stub: Option<&Stub>,
    pin: &PinnedProducer,
    rules: &Rules,
    run: &str,
) -> CommandSpec {
    let out = format!("{run}/out/index.scip");
    let mut env = BTreeMap::from([
        ("HOME".to_owned(), format!("{run}/home")),
        ("TMPDIR".to_owned(), format!("{run}/tmp")),
        ("LANG".to_owned(), lang().to_owned()),
    ]);
    let launcher = text(&resolution.launcher);
    let mut bin_links = Vec::new();
    let node_dir = resolution.node.as_ref().map(|node| node.dir.as_path());
    let argv: Vec<String>;
    let path: String;
    match (row.target, row.trust) {
        (Target::Stub, _) => {
            let mut v = vec![launcher];
            v.extend(stub.map(|stub| stub.args.clone()).unwrap_or_default());
            v.extend(["index".into(), "--output".into(), out]);
            argv = v;
            path = SYSTEM_PATH.join(":");
        }
        (Target::Real(ProducerName::ScipTypescript), _) => {
            let node = resolution.node.as_ref().map(|n| text(&n.path));
            argv = vec![
                node.unwrap_or_default(),
                launcher,
                "index".into(),
                "--output".into(),
                out,
                "--no-progress-bar".into(),
            ];
            path = path_with(&node_dir.into_iter().collect::<Vec<_>>());
        }
        (Target::Real(ProducerName::ScipPython), trust) => {
            let node = resolution.node.as_ref();
            let mut v = vec![
                node.map(|n| text(&n.path)).unwrap_or_default(),
                launcher,
                "index".into(),
                "--project-name".into(),
                rules
                    .arg("--project-name")
                    .unwrap_or(super::settings::PYTHON_PROJECT_NAME)
                    .to_owned(),
                // Without it scip-python runs `git`, which is not on the child's PATH, and
                // then fails on the undefined version.
                "--project-version".into(),
                rules
                    .arg("--project-version")
                    .unwrap_or(super::settings::PYTHON_PROJECT_VERSION)
                    .to_owned(),
                "--output".into(),
                out,
            ];
            if trust == TrustProfile::Restricted {
                v.extend([
                    "--environment".into(),
                    format!("{run}/producer-config/environment.json"),
                ]);
                path = format!("{run}/bin");
                if let Some(node) = node {
                    bin_links.push(BinLink {
                        name: "node".into(),
                        target: node.path.clone(),
                    });
                }
            } else {
                let dirs: Vec<&Path> = resolution
                    .python3
                    .iter()
                    .chain(resolution.node.iter())
                    .map(|exe| exe.dir.as_path())
                    .collect();
                path = path_with(&dirs);
            }
            argv = v;
        }
        (Target::Real(ProducerName::ScipJava), trust) => {
            let mut v = vec![launcher, "index".into(), "--output".into(), out];
            // The build tool the project's files select (#717), in the trusted profile.
            if let Some(tool) = rules.arg("--build-tool") {
                v.extend(["--build-tool".into(), tool.to_owned()]);
            }
            argv = v;
            let java_dir = resolution.java.as_ref().map(|java| java.dir.as_path());
            path = path_with(&java_dir.into_iter().collect::<Vec<_>>());
            if let Some(home) = &resolution.java_home {
                env.insert("JAVA_HOME".into(), text(home));
            }
            env.insert("COURSIER_CACHE".into(), text(&declared.coursier_cache));
            env.insert(
                "COURSIER_ARCHIVE_CACHE".into(),
                text(&declared.coursier_archive_cache),
            );
            match trust {
                TrustProfile::Restricted => {
                    env.insert("COURSIER_MODE".into(), "offline".into());
                }
                TrustProfile::Trusted => {
                    env.insert("GRADLE_OPTS".into(), "-Dorg.gradle.daemon=false".into());
                }
            }
        }
        (Target::Real(ProducerName::RustAnalyzer), _) => {
            let mut v = vec![
                launcher,
                "run".into(),
                rustup_toolchain(pin).to_owned(),
                "rust-analyzer".into(),
                "scip".into(),
                ".".into(),
                "--output".into(),
                out,
            ];
            // The project's settings (#717), when any is not rust-analyzer's default.
            let config = super::settings::RUST_ANALYZER_CONFIG;
            if rules.generated.iter().any(|file| file.path == config) {
                v.extend(["--config-path".into(), format!("{run}/{config}")]);
            }
            argv = v;
            let rustup_dir = resolution
                .rustup
                .as_ref()
                .map(|rustup| rustup.dir.as_path());
            path = path_with(&rustup_dir.into_iter().collect::<Vec<_>>());
            env.insert("RUSTUP_HOME".into(), text(&declared.rustup_home));
            env.insert("CARGO_HOME".into(), text(&declared.cargo_home));
            env.insert("CARGO_TARGET_DIR".into(), format!("{run}/target"));
            env.insert("RUSTUP_AUTO_INSTALL".into(), "0".into());
        }
    }
    env.insert("PATH".into(), path);
    CommandSpec {
        argv,
        env,
        bin_links,
    }
}

/// A file Trellis writes for the run, relative to the run directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Generated {
    pub path: String,
    pub bytes: Vec<u8>,
    /// A default under `worktree/`: written only when the retained revision has no root input
    /// at that worktree path, which then wins.
    pub default_only: bool,
}

/// The scip-typescript project config when the checkout has no root `tsconfig.json` (owner
/// ruling 2026-11-03, #652). `--infer-tsconfig` would write `{"compilerOptions":{"allowJs":true}}`,
/// whose ES5 default target cannot parse ES2015+ identifiers (an escaped astral name gave a
/// zero-width definition); this keeps its defaults but targets ES2022. With the ES2015+
/// target, `module` defaults to ES2015 and classic resolution, so `commonjs` keeps the
/// node10 resolution `--infer-tsconfig` had. `jsx: preserve` lets `.tsx` parse, and
/// `allowJs` keeps `.js` in the same program as `.ts` (#683).
pub const SCIP_TYPESCRIPT_TSCONFIG: &[u8] =
    br#"{"compilerOptions":{"allowJs":true,"jsx":"preserve","target":"es2022","module":"commonjs"}}"#;

/// A captured root input the row leaves out of the worktree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Omission {
    pub path: &'static str,
    pub reason: String,
}

/// The row's worktree omissions and additions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Rules {
    pub omit: Vec<Omission>,
    pub generated: Vec<Generated>,
    /// Producer flags and their values that the project's settings set (#717).
    pub args: Vec<(&'static str, String)>,
}

impl Rules {
    /// The value the settings give `flag`, if any.
    pub fn arg(&self, flag: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(name, _)| *name == flag)
            .map(|(_, value)| value.as_str())
    }
}

pub fn rules(row: &Row) -> Rules {
    match (row.target, row.trust) {
        (Target::Real(ProducerName::ScipJava), TrustProfile::Restricted) => Rules {
            omit: JAVA_BUILD_FILES
                .map(|path| Omission {
                    path,
                    reason: "java build logic is denied in the restricted profile".into(),
                })
                .to_vec(),
            generated: vec![Generated {
                path: "worktree/lsif-java.json".into(),
                bytes: br#"{"dependencies":[]}"#.to_vec(),
                default_only: false,
            }],
            args: Vec::new(),
        },
        (Target::Real(ProducerName::ScipPython), TrustProfile::Restricted) => Rules {
            omit: vec![Omission {
                path: "pyproject.toml",
                reason: "the only captured file that can configure the interpreter pyright uses"
                    .into(),
            }],
            generated: vec![Generated {
                path: "producer-config/environment.json".into(),
                bytes: b"[]".to_vec(),
                default_only: false,
            }],
            args: Vec::new(),
        },
        (Target::Real(ProducerName::ScipTypescript), _) => Rules {
            omit: Vec::new(),
            generated: vec![Generated {
                path: "worktree/tsconfig.json".into(),
                bytes: SCIP_TYPESCRIPT_TSCONFIG.to_vec(),
                default_only: true,
            }],
            args: Vec::new(),
        },
        _ => Rules::default(),
    }
}

/// Cache locations the row's environment declares to the producer.
pub fn declared_caches(row: &Row, declared: &Declared) -> Vec<(&'static str, PathBuf)> {
    match row.target {
        Target::Real(ProducerName::ScipJava) => vec![
            ("COURSIER_CACHE", declared.coursier_cache.clone()),
            (
                "COURSIER_ARCHIVE_CACHE",
                declared.coursier_archive_cache.clone(),
            ),
        ],
        Target::Real(ProducerName::RustAnalyzer) => vec![
            ("RUSTUP_HOME", declared.rustup_home.clone()),
            ("CARGO_HOME", declared.cargo_home.clone()),
        ],
        _ => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    Allow,
    Deny,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Enforcement {
    /// Trellis does not provide what the action needs.
    Structural,
    ProducerFlag,
    Advisory,
    NotEnforced,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Policy {
    pub decision: Decision,
    pub enforcement: Enforcement,
    pub detail: String,
}

fn policy(decision: Decision, enforcement: Enforcement, detail: &str) -> Policy {
    Policy {
        decision,
        enforcement,
        detail: detail.to_owned(),
    }
}

fn not_applicable() -> Policy {
    policy(
        Decision::NotApplicable,
        Enforcement::NotEnforced,
        "the producer has no such step",
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policies {
    pub network: Policy,
    pub package_manager: Policy,
    pub java_build_logic: Policy,
    pub annotation_processors: Policy,
    pub javascript_lifecycle_scripts: Policy,
    pub rust_build_scripts: Policy,
    pub rust_proc_macros: Policy,
    pub python_environment_inspection: Policy,
}

/// Allow or deny per policy; the default is deny. No policy claims a
/// filesystem or network sandbox.
pub fn policies(row: &Row) -> Policies {
    use Decision::{Allow, Deny};
    use Enforcement::{Advisory, NotEnforced, Structural};
    let restricted = row.trust == TrustProfile::Restricted;
    let mut all = Policies {
        network: policy(
            Deny,
            NotEnforced,
            "Trellis requests no download and does not isolate the network",
        ),
        package_manager: policy(
            Deny,
            Structural,
            "no package manager is invoked and no workspace-discovery flag is passed",
        ),
        java_build_logic: not_applicable(),
        annotation_processors: not_applicable(),
        javascript_lifecycle_scripts: not_applicable(),
        rust_build_scripts: not_applicable(),
        rust_proc_macros: not_applicable(),
        python_environment_inspection: not_applicable(),
    };
    match row.target {
        Target::Stub => {}
        Target::Real(ProducerName::ScipTypescript) => {
            all.javascript_lifecycle_scripts = policy(
                Deny,
                Structural,
                "no package manager runs, so no lifecycle script can",
            );
        }
        Target::Real(ProducerName::ScipPython) => {
            all.javascript_lifecycle_scripts = policy(
                Deny,
                Structural,
                "no package manager runs, so no lifecycle script can",
            );
            all.python_environment_inspection = if restricted {
                policy(
                    Deny,
                    Structural,
                    "child PATH is only the run's bin directory holding node; pyproject.toml is omitted; an empty --environment package list is supplied; not an OS sandbox, so an interpreter path hard-coded in the producer is not prevented",
                )
            } else {
                policy(
                    Allow,
                    NotEnforced,
                    "the declared python3 directory comes first on PATH, pyproject.toml is included and --environment is omitted",
                )
            };
        }
        Target::Real(ProducerName::ScipJava) => {
            if restricted {
                all.network = policy(
                    Deny,
                    Advisory,
                    "COURSIER_MODE=offline; a missing cached JDK fails the run instead of downloading",
                );
                all.package_manager = policy(
                    Deny,
                    Structural,
                    "Gradle and Maven build files are omitted, so scip-java cannot select either",
                );
                all.java_build_logic = policy(
                    Deny,
                    Structural,
                    "build files are omitted and a Trellis-written lsif-java.json declares no dependencies",
                );
                all.annotation_processors = policy(
                    Deny,
                    Structural,
                    "no dependency jars are supplied, so none can be discovered",
                );
            } else {
                all.network = policy(
                    Allow,
                    NotEnforced,
                    "Gradle or Maven may download dependencies into the run's private HOME and the declared Coursier caches",
                );
                all.package_manager = policy(
                    Allow,
                    NotEnforced,
                    "scip-java runs the project's Gradle or Maven build; GRADLE_OPTS=-Dorg.gradle.daemon=false",
                );
                all.java_build_logic = policy(
                    Allow,
                    NotEnforced,
                    "build files are included and their logic runs",
                );
                all.annotation_processors = policy(
                    Allow,
                    NotEnforced,
                    "the build resolves and runs annotation processors",
                );
            }
        }
        Target::Real(ProducerName::RustAnalyzer) => {
            all.network = policy(
                Allow,
                NotEnforced,
                "cargo may download crates into the declared CARGO_HOME; RUSTUP_AUTO_INSTALL=0 disables toolchain installs",
            );
            all.package_manager = policy(
                Allow,
                NotEnforced,
                "rust-analyzer runs cargo metadata and check",
            );
            all.rust_build_scripts = policy(
                Allow,
                NotEnforced,
                "rust-analyzer scip exposes no way to disable build scripts",
            );
            all.rust_proc_macros = policy(
                Allow,
                NotEnforced,
                "rust-analyzer scip exposes no way to disable proc macros",
            );
        }
    }
    all
}

/// The descriptor's own generation types, so the table and the retained `generation` object
/// cannot drift apart.
pub use crate::semantic_batch::descriptor::{
    GenerationComponent as Component, GenerationRuntime as RuntimeEntry,
};

/// What a generated batch will assert versus observe, exactly as the
/// descriptor's `generation` records it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationTable {
    pub trust_profile: Option<TrustProfile>,
    pub runtime: Vec<RuntimeEntry>,
    pub observed: Vec<Component>,
    pub asserted: Vec<Component>,
}

impl GenerationTable {
    /// `observed` and `asserted` partition the seven components, and
    /// `runtime` is non-empty exactly when runtime is observed.
    pub fn is_consistent(&self) -> bool {
        let mut all: Vec<Component> = self
            .observed
            .iter()
            .chain(&self.asserted)
            .copied()
            .collect();
        all.sort();
        let sorted_unique_names = self.runtime.windows(2).all(|w| w[0].name < w[1].name);
        all == Component::ALL
            && self.runtime.is_empty() != self.observed.contains(&Component::Runtime)
            && sorted_unique_names
    }
}

fn split(observed: &[Component]) -> (Vec<Component>, Vec<Component>) {
    (
        Component::ALL
            .into_iter()
            .filter(|c| observed.contains(c))
            .collect(),
        Component::ALL
            .into_iter()
            .filter(|c| !observed.contains(c))
            .collect(),
    )
}

/// The table row for a Trellis run of `row`.
pub fn run_generation(row: &Row, resolution: &Resolution) -> GenerationTable {
    use Component::{Config, DependencyManifests, ProducerExecutable, ProducerVersion, Source};
    let node = resolution
        .runtime_executables()
        .into_iter()
        .find(|exe: &RuntimeExecutable| exe.name == "node");
    let mut observed = vec![Source, Config, DependencyManifests];
    let mut runtime = Vec::new();
    match row.target {
        Target::Stub => {}
        Target::Real(ProducerName::ScipTypescript | ProducerName::ScipPython) => {
            observed.extend([ProducerVersion, Component::Runtime]);
            runtime.extend(node.map(|exe| RuntimeEntry {
                name: exe.name,
                sha256: exe.sha256,
            }));
        }
        Target::Real(ProducerName::ScipJava | ProducerName::RustAnalyzer) => {
            observed.extend([ProducerVersion, ProducerExecutable]);
        }
    }
    let (observed, asserted) = split(&observed);
    GenerationTable {
        trust_profile: Some(row.trust),
        runtime,
        observed,
        asserted,
    }
}

/// The table row for an owner-configured pickup: every component asserted.
pub fn pickup_generation() -> GenerationTable {
    let (observed, asserted) = split(&[]);
    GenerationTable {
        trust_profile: None,
        runtime: Vec::new(),
        observed,
        asserted,
    }
}
