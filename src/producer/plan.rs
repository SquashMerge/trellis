//! The producer plan (§3.2) and status reply. Planning reads and hashes files
//! and reads the retained native revision; it executes nothing and creates no
//! state. `planSha256` is the SHA-256 of the plan's canonical bytes.

use super::detect::{self, Declared, Environment, Resolution};
use super::exec::Limits;
use super::profiles::{self, GenerationTable, Row, Stub, Target};
use super::state::{RunRecord, StateDir};
use super::worktree::{self, RetainedInputs};
use super::{PRODUCER_UNAVAILABLE, TrustProfile};
use crate::native_ids::canonical;
use crate::scip_admission::PinnedProducer;
use crate::semantic_batch::descriptor::SourceManifestHasher;
use crate::store::{NativeReader, Store};
use anyhow::{Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const TIMEOUT_SECS: std::ops::RangeInclusive<u64> = 1..=14400;
pub const MEMORY_MIB: std::ops::RangeInclusive<u64> = 64..=65536;
pub const DEFAULT_TIMEOUT_SECS: u64 = 1800;
pub const DEFAULT_MEMORY_MIB: u64 = 8192;
/// Daemon operation payloads are closed and at most this large.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_EXECUTABLE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LimitsRequest {
    pub timeout_secs: u64,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: u64,
}

/// The `producer_plan` payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanRequest {
    pub version: u32,
    pub producer: String,
    pub trust_profile: TrustProfile,
    pub limits: LimitsRequest,
    pub executable: Option<String>,
    pub environment: Environment,
}

/// The `producer_status` payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusRequest {
    pub version: u32,
    pub run_id: Option<String>,
    pub environment: Environment,
}

fn bounded<T: for<'de> Deserialize<'de>>(payload: &Value) -> Result<T> {
    ensure!(
        serde_json::to_vec(payload)?.len() <= MAX_PAYLOAD_BYTES,
        "invalid_request: producer payload too large"
    );
    serde_json::from_value(payload.clone())
        .map_err(|_| anyhow::anyhow!("invalid_request: producer options"))
}

impl PlanRequest {
    pub fn parse(payload: &Value) -> Result<Self> {
        let request: Self = bounded(payload)?;
        ensure!(request.version == 1, "invalid_request: producer version");
        ensure!(
            TIMEOUT_SECS.contains(&request.limits.timeout_secs)
                && MEMORY_MIB.contains(&request.limits.memory_mib),
            "invalid_request: producer limits"
        );
        if let Some(executable) = &request.executable {
            ensure!(
                executable.len() <= MAX_EXECUTABLE_BYTES && Path::new(executable).is_absolute(),
                "invalid_request: executable"
            );
        }
        request.environment.validate()?;
        Ok(request)
    }
}

impl StatusRequest {
    pub fn parse(payload: &Value) -> Result<Self> {
        let request: Self = bounded(payload)?;
        ensure!(request.version == 1, "invalid_request: producer version");
        request.environment.validate()?;
        Ok(request)
    }
}

/// The retained revision's identity and inputs a plan binds to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Basis {
    pub source_set_id: String,
    pub root_id: String,
    pub source_revision: String,
    pub source_manifest_hash: String,
    pub toolchain_hash: String,
    pub config_hash: String,
    pub dependency_hash: String,
    pub documents: u64,
    pub requested_documents: u64,
    pub inputs: RetainedInputs,
}

/// Read the basis of the selected native revision. `language` counts the
/// documents the producer's artifact will cover.
pub fn read_basis(reader: &NativeReader<'_>, language: &str) -> Result<Basis> {
    let header = reader.header()?;
    let mut hasher = SourceManifestHasher::new(&header.source_set.id);
    let (mut documents, mut requested) = (0_u64, 0_u64);
    let mut inventory = reader.inventory()?;
    while let Some(document) = inventory.next_document()? {
        hasher.push(
            &document.key.language,
            &document.key.path,
            &document.content_hash,
        );
        documents += 1;
        requested += u64::from(document.key.language == language);
    }
    Ok(Basis {
        source_set_id: header.source_set.id,
        root_id: header.source_set.root_id,
        source_revision: header.revision.id,
        source_manifest_hash: hasher.finish(),
        toolchain_hash: header.revision.toolchain_hash,
        config_hash: header.revision.config_hash,
        dependency_hash: header.revision.dependency_hash,
        documents,
        requested_documents: requested,
        inputs: RetainedInputs::read(reader)?,
    })
}

/// SHA-256 of the canonical bytes of `plan` (which carries no hash field).
pub fn plan_sha256(plan: &Value) -> String {
    hex::encode(Sha256::digest(canonical(plan)))
}

/// `plan` with its `planSha256` added.
pub fn with_hash(mut plan: Value) -> Value {
    let hash = plan_sha256(&plan);
    plan["planSha256"] = json!(hash);
    plan
}

fn encoding_name(code: u8) -> &'static str {
    match code {
        1 => "utf8",
        2 => "utf16",
        _ => "unicodeScalar",
    }
}

fn listed(files: &[worktree::Listed]) -> Value {
    files
        .iter()
        .map(|file| json!({"path": file.path, "sha256": file.sha256}))
        .collect()
}

/// Everything a plan is made of, resolved.
pub struct Parts<'a> {
    pub workspace: &'a str,
    pub row: Row,
    pub pin: &'a PinnedProducer,
    pub resolution: &'a Resolution,
    pub declared: &'a Declared,
    pub stub: Option<&'a Stub>,
    pub basis: &'a Basis,
    pub limits: LimitsRequest,
    /// `<state dir>/runs/{runId}`: the literal placeholder stands for the
    /// run's ID.
    pub run_dir: &'a str,
}

/// The effective bounds of a run: the request's, over the defaults.
pub fn effective_limits(request: LimitsRequest) -> Limits {
    let limits = Limits::default();
    #[cfg(feature = "test-support")]
    let limits = limits.with_test_overrides();
    Limits {
        timeout: std::time::Duration::from_secs(request.timeout_secs),
        memory_bytes: request.memory_mib * 1024 * 1024,
        ..limits
    }
}

/// The plan without its hash.
pub fn build(parts: &Parts<'_>) -> Value {
    let row = &parts.row;
    let run = parts.run_dir;
    let spec = profiles::command(
        row,
        parts.resolution,
        parts.declared,
        parts.stub,
        parts.pin,
        run,
    );
    let rules = profiles::rules(row);
    let layout = worktree::layout(&parts.basis.inputs, &rules);
    let generation: GenerationTable = profiles::run_generation(row, parts.resolution);
    let limits = effective_limits(parts.limits);
    let caches: Vec<Value> = profiles::declared_caches(row, parts.declared)
        .into_iter()
        .map(|(name, path)| json!({"name": name, "path": path.to_string_lossy()}))
        .collect();
    json!({
        "workspace": parts.workspace,
        "producer": {
            "name": row.target.name(),
            "language": row.target.language(),
            "version": parts.pin.version,
            "pinnedExecutableHash": parts.pin.executable_hash,
            "positionEncoding": encoding_name(parts.pin.position_encoding),
            "launcher": parts.resolution.launcher.to_string_lossy(),
            "verification": {
                "method": parts.resolution.verification.method,
                "observed": parts.resolution.verification.observed,
                "result": "verified",
            },
        },
        "trustProfile": row.trust,
        "command": {
            "argv": spec.argv,
            "cwd": format!("{run}/worktree"),
            "env": spec.env,
        },
        "runtimeExecutables": parts.resolution.runtime_executables(),
        "generation": generation,
        "inputs": {
            "sourceSetId": parts.basis.source_set_id,
            "rootId": parts.basis.root_id,
            "sourceRevision": parts.basis.source_revision,
            "sourceManifestHash": parts.basis.source_manifest_hash,
            "components": {
                "toolchainHash": parts.basis.toolchain_hash,
                "configHash": parts.basis.config_hash,
                "dependencyHash": parts.basis.dependency_hash,
            },
            "documents": {
                "total": parts.basis.documents,
                "requestedLanguage": row.target.language(),
                "requested": parts.basis.requested_documents,
            },
            "config": listed(&layout.config),
            "dependencyManifests": listed(&layout.dependency_manifests),
            "omitted": layout
                .omitted
                .iter()
                .map(|(path, reason)| json!({"path": path, "reason": reason}))
                .collect::<Vec<_>>(),
            "generatedConfig": listed(&layout.generated),
            "declaredCaches": caches,
        },
        "policy": profiles::policies(row),
        "limits": {
            "timeoutSecs": parts.limits.timeout_secs,
            "memoryBytes": limits.memory_bytes,
            "memoryEnforcement": "sampled",
            "sampleMs": limits.sample.as_millis() as u64,
            "artifactBytes": limits.artifact_bytes,
            "logBytes": limits.log_bytes,
        },
        "output": {
            "runDirectory": run,
            "artifact": "out/index.scip",
            "publication": "semanticImportQueue",
        },
    })
}

/// `path` with its longest existing ancestor canonicalized, so a directory
/// not created yet is named as it will be once it is.
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut at = path;
    loop {
        if let Ok(resolved) = std::fs::canonicalize(at) {
            return missing
                .iter()
                .rev()
                .fold(resolved, |acc, part| acc.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                at = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `<state dir>/runs/{runId}` with the state directory as it will exist once created.
pub fn run_dir_template(store: &Store) -> String {
    let (roots, identity) = store.state_topology();
    let state = canonical_prefix(&roots.producer_state_dir(identity));
    format!("{}/runs/{{runId}}", state.to_string_lossy())
}

fn resolve_for(
    target: Target,
    trust: TrustProfile,
    declared: &Declared,
    executable: Option<&Path>,
    stub: Option<&Stub>,
) -> Result<(PinnedProducer, Resolution)> {
    let pin = target
        .pin()
        .ok_or_else(|| anyhow::anyhow!("{PRODUCER_UNAVAILABLE}: producer is not pinned"))?;
    let resolution = detect::resolve(target, trust, &pin, declared, executable, stub)
        .map_err(detect::Unavailable::into_error)?;
    Ok((pin, resolution))
}

/// A plan's inputs, resolved and read but not yet rendered.
pub struct Prepared {
    pub row: Row,
    pub pin: PinnedProducer,
    pub resolution: Resolution,
    pub declared: Declared,
    pub basis: Basis,
    pub limits: LimitsRequest,
}

impl Prepared {
    /// The plan without its hash, with `run_dir` standing for the run directory.
    pub fn build(&self, store: &Store, stub: Option<&Stub>, run_dir: &str) -> Value {
        build(&Parts {
            workspace: store.workspace_root(),
            row: self.row,
            pin: &self.pin,
            resolution: &self.resolution,
            declared: &self.declared,
            stub,
            basis: &self.basis,
            limits: self.limits,
            run_dir,
        })
    }
}

/// Resolve and verify the producer and read the retained revision a plan binds to.
/// Refusals and unavailable producers are typed errors; nothing is created or executed.
pub fn prepare(store: &Store, request: &PlanRequest, stub: Option<&Stub>) -> Result<Prepared> {
    let target = Target::parse(&request.producer, stub.is_some())
        .ok_or_else(|| anyhow::anyhow!("invalid_request: unknown producer"))?;
    let row = profiles::row(target, request.trust_profile)?;
    let declared = Declared::new(&request.environment)?;
    let executable = request.executable.as_deref().map(Path::new);
    let (pin, resolution) = resolve_for(target, row.trust, &declared, executable, stub)?;
    let baseline = store.index_baseline()?;
    let basis =
        store.with_native_reader(baseline, |reader| read_basis(reader, target.language()))?;
    Ok(Prepared {
        row,
        pin,
        resolution,
        declared,
        basis,
        limits: request.limits,
    })
}

/// The plan for `request`, with `planSha256`.
pub fn plan(store: &Store, request: &PlanRequest, stub: Option<&Stub>) -> Result<Value> {
    let prepared = prepare(store, request, stub)?;
    let run_dir = run_dir_template(store);
    Ok(with_hash(prepared.build(store, stub, &run_dir)))
}

fn availability(result: &Result<Resolution, detect::Unavailable>) -> Value {
    match result {
        Ok(_) => json!({"result": "available", "reason": null}),
        Err(reason) => json!({"result": "unavailable", "reason": reason.0}),
    }
}

fn last_run(record: &RunRecord) -> Value {
    json!({
        "runId": record.run_id,
        "trustProfile": record.trust_profile,
        "state": record.state,
        "outcome": record.outcome,
        "errorCode": record.error_code,
        "planSha256": record.plan_sha256,
        "sourceRevision": record.source_revision,
        "createdAt": record.created_at,
        "finishedAt": record.finished_at,
    })
}

/// `restricted` or `trusted` when the newest run of the producer imported
/// under that profile; otherwise the producer is imported-only.
fn mode(last: Option<&RunRecord>) -> Value {
    match last {
        Some(record) if record.outcome.as_deref() == Some("imported") => {
            json!(record.trust_profile)
        }
        _ => json!("importedOnly"),
    }
}

/// The status reply: detection, trust-profile availability and mode per
/// producer, and one run's record when `runId` is given. `activeRun` and `pickup` are
/// filled by their owners.
pub fn status(store: &Store, request: &StatusRequest, stub: Option<&Stub>) -> Result<Value> {
    let declared = Declared::new(&request.environment)?;
    let head = match store.index_baseline() {
        Ok(pin) => {
            let revision =
                store.with_native_reader(pin, |reader| Ok(reader.header()?.revision.id))?;
            Some(json!({
                "sourceRevision": revision,
                "indexGeneration": pin.index_generation.to_string(),
                "indexRevision": pin.index_revision,
            }))
        }
        Err(error) if error.to_string().starts_with("index_not_ready") => None,
        Err(error) => return Err(error),
    };
    let (roots, identity) = store.state_topology();
    let runs = match StateDir::existing(roots, identity)? {
        Some(state) => state.runs()?,
        None => Vec::new(),
    };
    let producers: Vec<Value> = Target::all(stub.is_some())
        .into_iter()
        .map(|target| {
            let pin = target.pin();
            let base = match &pin {
                Some(pin) => detect::resolve_base(target, pin, &declared, None, stub),
                None => Err(detect::Unavailable("producer is not pinned".into())),
            };
            let trust = |trust: TrustProfile| -> Value {
                if let Some(reason) = profiles::refusal(target, trust) {
                    return json!({"result": super::PROFILE_UNSUPPORTED, "reason": reason});
                }
                match (&pin, &base) {
                    (Some(pin), Ok(_))
                        if target == Target::Real(super::ProducerName::ScipPython)
                            && trust == TrustProfile::Trusted =>
                    {
                        availability(&detect::resolve(target, trust, pin, &declared, None, stub))
                    }
                    _ => availability(&base),
                }
            };
            let last = runs.iter().find(|record| record.producer == target.name());
            json!({
                "name": target.name(),
                "language": target.language(),
                "detection": availability(&base),
                "trustProfiles": {
                    "restricted": trust(TrustProfile::Restricted),
                    "trusted": trust(TrustProfile::Trusted),
                },
                "mode": mode(last),
                "lastRun": last.map(last_run),
                "pickup": null,
            })
        })
        .collect();
    let mut reply = json!({
        "workspace": store.workspace_root(),
        "head": head,
        "activeRun": null,
        "producers": producers,
    });
    if let Some(run_id) = &request.run_id {
        let record = match StateDir::existing(roots, identity)? {
            Some(state) => state.read_run_record(run_id)?,
            None => None,
        };
        let record = record.ok_or_else(|| anyhow::anyhow!("{}: run", super::NOT_FOUND))?;
        reply["run"] = serde_json::to_value(record)?;
    }
    Ok(reply)
}
