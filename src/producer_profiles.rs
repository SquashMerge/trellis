//! The static producer-context vocabulary (S15; section 12 ruling 1 of the 0.2.x refactoring
//! spec: like the pin, this is vocabulary). The four pinned producers and the test stub's id,
//! the two trust profiles, the closed producer x trust-profile table (`Target`, `Row`, `row`)
//! with each row's worktree omissions and generated files (`rules`), and `context_hash`, the
//! controlled-profile input digest. Static data and pure functions: no file, environment,
//! worktree or database read; `context_hash` takes the source test as the caller's closure.
//! Storage validates stored producer context against this table at publish time
//! (`store::native_first::context`); producer commands, caches, settings and the controlled
//! worktree stay in `crate::producer`, which re-exports these items at their old paths.
use crate::producer_pin::PinnedProducer;
use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

pub const PROFILE_UNSUPPORTED: &str = "profile_unsupported";

/// The four producers pinned in the reference hardware profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProducerName {
    #[serde(rename = "scip-java")]
    ScipJava,
    #[serde(rename = "scip-typescript")]
    ScipTypescript,
    #[serde(rename = "scip-python")]
    ScipPython,
    #[serde(rename = "rust-analyzer")]
    RustAnalyzer,
}

impl ProducerName {
    pub const ALL: [Self; 4] = [
        Self::ScipJava,
        Self::ScipTypescript,
        Self::ScipPython,
        Self::RustAnalyzer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScipJava => "scip-java",
            Self::ScipTypescript => "scip-typescript",
            Self::ScipPython => "scip-python",
            Self::RustAnalyzer => "rust-analyzer",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// `restricted` (the default) runs no repository code; `trusted` allows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrustProfile {
    Restricted,
    Trusted,
}

/// The id of the test-support stand-in producer.
pub const STUB_PRODUCER: &str = "trellis-synthetic-scip";

/// The pin id of the test-support stand-in producer.
pub const STUB_NAME: &str = crate::producer_profiles::STUB_PRODUCER;
const RUST_REFUSAL: &str = "rust-analyzer cannot disable build scripts or proc macros";
/// The root build files and, as `*/` patterns, the nested module build files capture retains
/// above Java sources (#854).
const JAVA_BUILD_FILES: [&str; 9] = [
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
    "*/pom.xml",
    "*/build.gradle",
    "*/build.gradle.kts",
];

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
        crate::producer_pin::pinned_producer_by_id(self.name())
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

/// A captured root input the row leaves out of the worktree. A `path` of `*/name` leaves out
/// every nested input of that file name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Omission {
    pub path: &'static str,
    pub reason: String,
}

impl Omission {
    /// Whether this omission leaves out the input at `path`.
    pub fn matches(&self, path: &str) -> bool {
        match self.path.strip_prefix('*') {
            Some(suffix) => path.ends_with(suffix),
            None => self.path == path,
        }
    }
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
                .into_iter()
                .chain([Omission {
                    path: "lsif-java.json",
                    reason: "the restricted profile writes its own lsif-java.json; the checkout's can name dependencies to fetch".into(),
                }])
                .collect(),
            generated: vec![Generated {
                path: "worktree/lsif-java.json".into(),
                bytes: br#"{"dependencies":[]}"#.to_vec(),
                default_only: false,
            }],
            args: Vec::new(),
        },
        (Target::Real(ProducerName::ScipPython), TrustProfile::Restricted) => Rules {
            omit: ["pyproject.toml", "pyrightconfig.json"]
                .map(|path| Omission {
                    path,
                    reason: "a captured file that can configure the interpreter pyright uses"
                        .into(),
                })
                .to_vec(),
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

pub(crate) fn omission<'a>(rules: &'a Rules, path: &str) -> Option<&'a Omission> {
    rules.omit.iter().find(|omission| omission.matches(path))
}

/// Sources, config, then dependency inputs claim a path only once.
pub(crate) fn supplied(rules: &Rules, path: &str, already_present: bool) -> bool {
    omission(rules, path).is_none() && !already_present
}

/// Whether a default-only generated file gives way to a root input at its worktree path.
pub(crate) fn replaced(file: &Generated, present: impl Fn(&str) -> bool) -> bool {
    file.default_only && file.path.strip_prefix("worktree/").is_some_and(present)
}

/// A controlled profile's effective non-source context. Native identity and freshness
/// keep their full captured hashes; this digest is only for P-to-N pending eligibility.
/// Entries borrow retained metadata, including encoded bytes, without decoding file bodies.
pub(crate) fn context_hash(
    rules: &Rules,
    config: &[Value],
    dependencies: &[Value],
    is_source: &dyn Fn(&str) -> Result<bool>,
    cancel: &AtomicBool,
) -> Result<String> {
    let mut files = BTreeMap::new();
    let mut metadata = Vec::new();
    for entry in config.iter().chain(dependencies) {
        check_cancel(cancel)?;
        let Some(path) = entry["path"].as_str() else {
            metadata.push(entry);
            continue;
        };
        if omission(rules, path).is_some() {
            continue;
        }
        match entry["type"].as_str() {
            Some("absent") => {}
            Some("present") => {
                if supplied(rules, path, files.contains_key(path)) && !is_source(path)? {
                    files.insert(path, entry);
                }
            }
            _ => metadata.push(entry),
        }
    }
    let mut hash = Sha256::new();
    hash.update(b"controlled-worktree-context:v1\0files[");
    for entry in files.values() {
        check_cancel(cancel)?;
        crate::native_ids::canonical_into(entry, &mut |bytes| hash.update(bytes));
        hash.update(b",");
    }
    hash.update(b"]metadata[");
    for entry in metadata {
        check_cancel(cancel)?;
        crate::native_ids::canonical_into(entry, &mut |bytes| hash.update(bytes));
        hash.update(b",");
    }
    hash.update(b"]generated[");
    for file in &rules.generated {
        check_cancel(cancel)?;
        let present = if file.default_only {
            match file.path.strip_prefix("worktree/") {
                Some(path) if omission(rules, path).is_none() => {
                    files.contains_key(path) || is_source(path)?
                }
                _ => false,
            }
        } else {
            false
        };
        if !replaced(file, |_| present) {
            crate::native_ids::canonical_string_into(&file.path, &mut |bytes| hash.update(bytes));
            hash.update(Sha256::digest(&file.bytes));
            hash.update(b",");
        }
    }
    hash.update(b"]");
    check_cancel(cancel)?;
    Ok(hex::encode(hash.finalize()))
}

pub(crate) fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::SeqCst), "cancelled");
    Ok(())
}
