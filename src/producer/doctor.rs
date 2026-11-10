//! `trellis producer doctor` (#880). Read-only: for each producer it reports the state that
//! `producer plan` verification finds, through the same declared environment and
//! [`detect::resolve`], the prerequisites, and the pinned install command for one that is not
//! ready. Every check is a file read or a hash; nothing here executes anything, and nothing
//! here installs. `ready` therefore means a plan for the producer will verify.

use super::detect::{self, Declared, UnavailableKind};
use super::profiles::{self, Target};
use super::{ProducerName, TrustProfile};
use serde::Serialize;
use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

/// What works with no producer installed.
pub const NATIVE_ONLY: &str = "Without producers, Trellis indexes every language with native \
     (syntax) evidence only: symbols, written calls and references. Producers are optional and \
     add semantic evidence, such as resolved call targets.";

/// The JDK restricted scip-java runs on: its default JVM, fetched into the coursier archive
/// cache beforehand because restricted runs are offline (producer-provisioning.md).
const SCIP_JAVA_JDK_RELEASE: &str =
    "https/github.com/adoptium/temurin17-binaries/releases/download/jdk-17.0.20.1%252B1";
const SCIP_JAVA_JDK: &str = "jdk-17.0.20.1+1";
/// Entries of the release directory looked at for the unpacked JDK.
const MAX_CACHE_ENTRIES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum State {
    Ready,
    Missing,
    WrongVersion,
    Unverifiable,
    PrerequisiteMissing,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::WrongVersion => "wrongVersion",
            Self::Unverifiable => "unverifiable",
            Self::PrerequisiteMissing => "prerequisiteMissing",
        }
    }
}

/// A runtime or tool a producer needs, and whether the declared environment has it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prerequisite {
    pub name: String,
    pub present: bool,
    /// `run` (a plan or run needs it), `install` (the pinned install command needs it) or
    /// `restrictedRun`.
    pub needed_for: Vec<&'static str>,
    pub detail: String,
}

/// One producer's doctor entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProducerReport {
    pub name: &'static str,
    pub languages: &'static [&'static str],
    /// Whether the checkout has sources in one of `languages`; `true` when they are unknown.
    pub relevant: bool,
    /// The profile this check stands for: `restricted`, or `trusted` for rust-analyzer, which
    /// only runs trusted.
    pub trust_profile: TrustProfile,
    pub state: State,
    pub pinned: String,
    /// The version found, for `wrongVersion`.
    pub found: Option<String>,
    /// Why the producer is not ready.
    pub reason: Option<String>,
    /// The pinned install command, for a producer that is not ready.
    pub install_command: Option<String>,
    pub prerequisites: Vec<Prerequisite>,
}

/// The whole doctor reply.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub workspace: Option<String>,
    /// Languages with sources in the checkout, as an index capture would admit them.
    pub languages: Option<Vec<&'static str>>,
    pub languages_error: Option<String>,
    pub native_evidence: &'static str,
    pub producers: Vec<ProducerReport>,
}

/// A pinned install command: the argv `producer install` runs, and nothing else.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallCommand {
    pub argv: Vec<String>,
}

impl InstallCommand {
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// The tool each producer's pinned install command runs.
fn install_tool(producer: ProducerName) -> &'static str {
    match producer {
        ProducerName::ScipTypescript | ProducerName::ScipPython => "npm",
        ProducerName::ScipJava => "coursier",
        ProducerName::RustAnalyzer => "rustup",
    }
}

/// The pinned install command for `producer`. The npm producers and scip-java run the
/// reference profile row's `installMethod` as captured. The rust-analyzer row records the
/// component and toolchain in prose, so its command adds the component to the pinned
/// toolchain, as the provisioning table says.
pub fn pinned_install(producer: ProducerName) -> Option<InstallCommand> {
    let pin = Target::Real(producer).pin()?;
    let argv: Vec<String> = match producer {
        ProducerName::RustAnalyzer => [
            "rustup",
            "component",
            "add",
            "rust-analyzer",
            "--toolchain",
            detect::rustup_toolchain(&pin),
        ]
        .map(str::to_owned)
        .to_vec(),
        _ => crate::scip_admission::reference_install_method(producer.as_str())?
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    };
    // A row whose method is not a command for the expected tool, or does not name the pin,
    // has no install command.
    let version = detect::rustup_toolchain(&pin);
    (argv.first().map(String::as_str) == Some(install_tool(producer))
        && argv.iter().any(|arg| arg.ends_with(version)))
    .then_some(InstallCommand { argv })
}

/// The first declared `PATH` directory's executable `name`, as resolution finds it.
pub fn on_path(name: &str, declared: &Declared) -> Option<PathBuf> {
    detect::find_on_path(name, &declared.path_dirs).map(|dir| dir.join(name))
}

fn rust_toolchain_dir(declared: &Declared) -> Option<PathBuf> {
    let pin = Target::Real(ProducerName::RustAnalyzer).pin()?;
    Some(declared.rustup_home.join("toolchains").join(format!(
        "{}-{}",
        detect::rustup_toolchain(&pin),
        crate::scip_admission::host_target()
    )))
}

fn scip_java_jdk_cached(declared: &Declared) -> bool {
    let Ok(entries) = fs::read_dir(declared.coursier_archive_cache.join(SCIP_JAVA_JDK_RELEASE))
    else {
        return false;
    };
    entries
        .take(MAX_CACHE_ENTRIES)
        .flatten()
        .any(|entry| entry.path().join(SCIP_JAVA_JDK).is_dir())
}

fn tool(name: &str, needed_for: Vec<&'static str>, declared: &Declared) -> Prerequisite {
    let found = on_path(name, declared);
    Prerequisite {
        name: name.to_owned(),
        present: found.is_some(),
        needed_for,
        detail: match found {
            Some(path) => path.display().to_string(),
            None => format!("no {name} on the declared PATH"),
        },
    }
}

fn prerequisites(producer: ProducerName, declared: &Declared) -> Vec<Prerequisite> {
    match producer {
        ProducerName::ScipTypescript | ProducerName::ScipPython => vec![
            tool("node", vec!["run", "install"], declared),
            tool("npm", vec!["install"], declared),
        ],
        ProducerName::ScipJava => {
            let cached = scip_java_jdk_cached(declared);
            vec![
                tool("java", vec!["run"], declared),
                tool("coursier", vec!["install"], declared),
                Prerequisite {
                    name: "scip-java JDK".to_owned(),
                    present: cached,
                    needed_for: vec!["restrictedRun"],
                    detail: if cached {
                        format!("{SCIP_JAVA_JDK} in the coursier archive cache")
                    } else {
                        format!(
                            "no {SCIP_JAVA_JDK} in the coursier archive cache {}; restricted \
                             runs are offline and fetch nothing",
                            declared.coursier_archive_cache.display()
                        )
                    },
                },
            ]
        }
        ProducerName::RustAnalyzer => {
            let toolchain = rust_toolchain_dir(declared);
            let present = toolchain.as_deref().is_some_and(Path::is_dir);
            vec![
                tool("rustup", vec!["run", "install"], declared),
                Prerequisite {
                    name: "rust toolchain".to_owned(),
                    present,
                    needed_for: vec!["run", "install"],
                    detail: match toolchain {
                        Some(dir) if present => dir.display().to_string(),
                        Some(dir) => format!("no toolchain at {}", dir.display()),
                        None => "rust-analyzer is not pinned".to_owned(),
                    },
                },
            ]
        }
    }
}

/// The trust profile a doctor check stands for: the default `restricted`, unless the
/// producer has no restricted row.
fn check_profile(target: Target) -> TrustProfile {
    match profiles::row(target, TrustProfile::Restricted) {
        Ok(_) => TrustProfile::Restricted,
        Err(_) => TrustProfile::Trusted,
    }
}

/// The doctor entry for one producer, with `relevant` set.
pub fn check(producer: ProducerName, declared: &Declared) -> ProducerReport {
    let target = Target::Real(producer);
    let trust = check_profile(target);
    let prerequisites = prerequisites(producer, declared);
    let pin = target.pin();
    let pinned = pin
        .as_ref()
        .map(|pin| pin.version.clone())
        .unwrap_or_default();
    let (state, found, reason) = match &pin {
        None => (
            State::Unverifiable,
            None,
            Some("producer is not pinned".to_owned()),
        ),
        // The same resolution `producer plan` verifies with (no `--executable`).
        Some(pin) => match detect::resolve(target, trust, pin, declared, None, None) {
            Ok(_) => match prerequisites
                .iter()
                .find(|need| !need.present && need.needed_for.contains(&"restrictedRun"))
            {
                Some(need) => (State::PrerequisiteMissing, None, Some(need.detail.clone())),
                None => (State::Ready, None, None),
            },
            Err(detect::Unavailable(reason, kind)) => {
                let state = match kind {
                    UnavailableKind::Missing => State::Missing,
                    UnavailableKind::WrongVersion { .. } => State::WrongVersion,
                    UnavailableKind::Unverifiable => State::Unverifiable,
                    // Resolution looks for rustup before the binary: with neither, the
                    // producer itself is missing.
                    UnavailableKind::PrerequisiteMissing
                        if producer == ProducerName::RustAnalyzer
                            && !rust_toolchain_dir(declared)
                                .is_some_and(|dir| dir.join("bin/rust-analyzer").is_file()) =>
                    {
                        State::Missing
                    }
                    UnavailableKind::PrerequisiteMissing => State::PrerequisiteMissing,
                };
                let found = match kind {
                    UnavailableKind::WrongVersion { found } => Some(found),
                    _ => None,
                };
                (state, found, Some(reason))
            }
        },
    };
    let install_command = (state != State::Ready)
        .then(|| pinned_install(producer).map(|command| command.display()))
        .flatten();
    ProducerReport {
        name: producer.as_str(),
        languages: target.languages(),
        relevant: true,
        trust_profile: trust,
        state,
        pinned,
        found,
        reason,
        install_command,
        prerequisites,
    }
}

/// The doctor reply for `workspace` (when one was found) under `declared`.
pub fn report(workspace: Option<&Path>, declared: &Declared) -> Report {
    let (languages, languages_error) = match workspace {
        None => (None, Some("no checkout found".to_owned())),
        Some(root) => match crate::capture::languages_present(root) {
            Ok(found) => (Some(found.into_iter().collect::<Vec<_>>()), None),
            Err(error) => (None, Some(format!("{error:#}"))),
        },
    };
    let producers = ProducerName::ALL
        .into_iter()
        .map(|producer| {
            let mut entry = check(producer, declared);
            entry.relevant = languages.as_ref().is_none_or(|present| {
                entry
                    .languages
                    .iter()
                    .any(|language| present.contains(language))
            });
            entry
        })
        .collect();
    Report {
        workspace: workspace.map(|root| root.display().to_string()),
        languages,
        languages_error,
        native_evidence: NATIVE_ONLY,
        producers,
    }
}

/// One producer's entry as text lines.
pub fn render_producer(entry: &ProducerReport) -> String {
    let mut out = String::new();
    let state = match (&entry.state, &entry.found) {
        (State::WrongVersion, Some(found)) => {
            format!("wrongVersion (found {found}, pinned {})", entry.pinned)
        }
        (State::Ready, _) => format!("ready ({})", entry.pinned),
        (state, _) => state.as_str().to_owned(),
    };
    let _ = writeln!(
        out,
        "{} ({}, {}): {state}",
        entry.name,
        entry.languages.join(", "),
        match entry.trust_profile {
            TrustProfile::Restricted => "restricted",
            TrustProfile::Trusted => "trusted",
        }
    );
    if let Some(reason) = &entry.reason {
        let _ = writeln!(out, "  reason: {reason}");
    }
    if let Some(command) = &entry.install_command {
        let _ = writeln!(out, "  install: {command}");
        let _ = writeln!(out, "    or: trellis producer install {}", entry.name);
    }
    for need in entry.prerequisites.iter().filter(|need| !need.present) {
        let _ = writeln!(
            out,
            "  missing prerequisite {} (for {}): {}",
            need.name,
            need.needed_for.join(", "),
            need.detail
        );
    }
    out
}

/// The doctor reply as text.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    match &report.workspace {
        Some(root) => {
            let _ = writeln!(out, "Checkout: {root}");
        }
        None => {
            let _ = writeln!(out, "Checkout: none found");
        }
    }
    match (&report.languages, &report.languages_error) {
        (Some(languages), _) if languages.is_empty() => {
            let _ = writeln!(out, "Languages: none");
        }
        (Some(languages), _) => {
            let _ = writeln!(out, "Languages: {}", languages.join(", "));
        }
        (None, error) => {
            let _ = writeln!(
                out,
                "Languages: unknown ({}); every producer is listed",
                error.as_deref().unwrap_or("not read")
            );
        }
    }
    let _ = writeln!(out, "{}", report.native_evidence);
    let _ = writeln!(out);
    for entry in report.producers.iter().filter(|entry| entry.relevant) {
        out.push_str(&render_producer(entry));
    }
    let other: Vec<&str> = report
        .producers
        .iter()
        .filter(|entry| !entry.relevant)
        .map(|entry| entry.name)
        .collect();
    if !other.is_empty() {
        let _ = writeln!(out, "Not needed for this checkout: {}", other.join(", "));
    }
    let _ = writeln!(
        out,
        "Trellis installs nothing on its own; `trellis producer install <producer>` runs the \
         pinned command only when you ask."
    );
    out
}
