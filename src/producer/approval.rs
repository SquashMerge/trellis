//! The approval of automatic producer runs (#660, D2, D3). A manual run that imports leaves
//! `approvals/<producer>.json` in the checkout's producer state: the request the operator
//! inspected and the hash of its template. The checkout's `AutoRunOwner` re-runs the producer
//! only against a record that still matches the checkout it was written for and a plan whose
//! template is unchanged; it never writes or refreshes one.

use super::TrustProfile;
use super::detect::Environment;
use super::plan::{self, LimitsRequest, PlanRequest, Prepared};
use super::profiles::Stub;
use super::run::{RunRequest, SupersedeToken, typed_code};
use super::state::{StateDir, now_millis};
use crate::store::{Store, topology::WorkspaceIdentity};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The part of the manual request an automatic run reuses (SD16): settings are not stored, so
/// an automatic run derives them at the head.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalRequest {
    pub limits: LimitsRequest,
    pub executable: Option<String>,
    pub environment: Environment,
}

/// `approvals/<producer>.json`, bound to the checkout it was written for. The device is never
/// stored: it changes across a remount that leaves the checkout the same.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Approval {
    pub producer: String,
    pub trust_profile: TrustProfile,
    pub request: ApprovalRequest,
    pub template_sha256: String,
    pub root_record_id: String,
    pub root_inode: u64,
    pub approved_at: u64,
}

/// What reading a producer's approval found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalRead {
    Absent,
    /// It failed its closed parse, was too large, sat in unsafe state, or could not be read.
    Unreadable,
    /// It was written for another root: the record ID or the inode differs.
    OtherRoot,
    Valid(Box<Approval>),
}

impl ApprovalRead {
    /// The `needsApproval` reason; `None` for a valid record.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Absent => Some("no approval record"),
            Self::Unreadable => Some("approval record unreadable"),
            Self::OtherRoot => Some("approval belongs to another root"),
            Self::Valid(_) => None,
        }
    }
}

/// Why an approved plan was not turned into a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// The head is no longer the revision the caller decided on.
    HeadMoved,
    /// The template differs from the one approved: `approval template changed`.
    Template,
    /// Planning failed with this typed code.
    Failed(String),
}

/// The record for a manual run that imported `prepared` under `request`.
pub fn record(
    request: &PlanRequest,
    prepared: &Prepared,
    identity: &WorkspaceIdentity,
) -> Approval {
    Approval {
        producer: prepared.row.target.name().to_owned(),
        trust_profile: prepared.row.trust,
        request: ApprovalRequest {
            limits: request.limits,
            executable: request.executable.clone(),
            environment: request.environment.clone(),
        },
        template_sha256: plan::template_sha256(prepared, &request.environment),
        root_record_id: identity.record_id.clone(),
        root_inode: identity.inode,
        approved_at: now_millis(),
    }
}

/// Replace the producer's approval record.
pub fn write(state: &StateDir, approval: &Approval) -> Result<()> {
    state.write_approval_bytes(&approval.producer, &serde_json::to_vec(approval)?)
}

/// The approval of `producer` for the checkout `identity`.
pub fn read(state: &StateDir, producer: &str, identity: &WorkspaceIdentity) -> ApprovalRead {
    let bytes = match state.read_approval_bytes(producer) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return ApprovalRead::Absent,
        Err(_) => return ApprovalRead::Unreadable,
    };
    match serde_json::from_slice::<Approval>(&bytes) {
        Ok(approval) if approval.producer == producer => {
            if approval.root_record_id == identity.record_id
                && approval.root_inode == identity.inode
            {
                ApprovalRead::Valid(Box::new(approval))
            } else {
                ApprovalRead::OtherRoot
            }
        }
        _ => ApprovalRead::Unreadable,
    }
}

/// The automatic run `approval` allows at `expected_revision`. The request is rebuilt from the
/// record alone and re-parsed through the manual path's bounds; the trust profile comes only
/// from the record. The head must still be `expected_revision`, and the plan's template must
/// still hash to the approved one.
pub fn run_request(
    store: &Store,
    approval: &Approval,
    stub: Option<&Stub>,
    expected_revision: &str,
    token: SupersedeToken,
) -> Result<RunRequest, Refusal> {
    let failed = |error: anyhow::Error| Refusal::Failed(typed_code(&error).0);
    let request = PlanRequest::parse(&json!({
        "version": 1,
        "producer": approval.producer,
        "trustProfile": approval.trust_profile,
        "limits": approval.request.limits,
        "executable": approval.request.executable,
        "environment": approval.request.environment,
    }))
    .map_err(failed)?;
    let prepared = plan::prepare(store, &request, stub).map_err(failed)?;
    if prepared.basis.source_revision != expected_revision {
        return Err(Refusal::HeadMoved);
    }
    if plan::template_sha256(&prepared, &request.environment) != approval.template_sha256 {
        return Err(Refusal::Template);
    }
    let planned = prepared.build(store, stub, &plan::run_dir_template(store));
    Ok(RunRequest::automatic(
        request,
        plan::plan_sha256(&planned),
        token,
    ))
}
