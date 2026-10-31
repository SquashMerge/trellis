use crate::{
    model::{AnchorReason, AnchorResult, DurableAnchor},
    native_evidence::{Declaration, Header},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

const HEADER_DOMAIN: &[u8] = b"trellis.header.v1\0";
const GROUP_DOMAIN: &[u8] = b"trellis.sibling-group.v1\0";

pub fn header_hash(header: &Header) -> Result<String> {
    let value = serde_json::to_value(header)?;
    Ok(crate::native_ids::digest(
        HEADER_DOMAIN,
        &crate::native_ids::canonical(&value),
    ))
}

pub fn sibling_group_hash(headers: &[String]) -> Result<String> {
    let value = serde_json::json!({"headers": headers});
    Ok(crate::native_ids::digest(
        GROUP_DOMAIN,
        &crate::native_ids::canonical(&value),
    ))
}

fn same_group(left: &Declaration, right: &Declaration) -> bool {
    left.document == right.document
        && left.ancestors == right.ancestors
        && left.kind == right.kind
        && left.name == right.name
        && left.key.signature == right.key.signature
}

fn measured_group<'a>(
    focused: &Declaration,
    declarations: &'a [Declaration],
) -> Result<Vec<&'a Declaration>> {
    let mut group: Vec<_> = declarations
        .iter()
        .filter(|row| same_group(focused, row))
        .collect();
    // The native validator already proved sibling ordinals, including Decision 0019's late
    // ordinals for declarations inside measured Rust macros; use them as the group order.
    group.sort_by_key(|row| row.key.ordinal);
    ensure!(
        !group.is_empty() && group.iter().any(|row| row.syntax_id == focused.syntax_id),
        "invalid anchor sibling group"
    );
    for (ordinal, row) in group.iter().enumerate() {
        ensure!(row.key.ordinal == ordinal, "invalid anchor sibling ordinal");
        ensure!(
            row.revision_id == focused.revision_id,
            "invalid anchor sibling revision"
        );
    }
    Ok(group)
}

pub fn capture_anchor(
    focused: &Declaration,
    declarations: &[Declaration],
) -> Result<DurableAnchor> {
    let group = measured_group(focused, declarations)?;
    let focused_header_hash = header_hash(&focused.header)?;
    let headers = group
        .iter()
        .map(|row| header_hash(&row.header))
        .collect::<Result<Vec<_>>>()?;
    let anchor = DurableAnchor {
        syntax_id: focused.syntax_id.clone(),
        document: focused.document.clone(),
        captured_revision_id: focused.revision_id.clone(),
        header_hash: focused_header_hash.clone(),
        sibling_group_hash: sibling_group_hash(&headers)?,
        sibling_count: headers.len(),
        identical_header_count: headers
            .iter()
            .filter(|hash| **hash == focused_header_hash)
            .count(),
    };
    anchor.validate()?;
    Ok(anchor)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ContinuityState {
    Unchanged,
    Changed,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GroupContinuity {
    pub from_revision_id: String,
    pub to_revision_id: String,
    pub state: ContinuityState,
    pub evidence: Option<String>,
}
impl GroupContinuity {
    fn validate(&self, anchor: &DurableAnchor, current_revision_id: &str) -> Result<()> {
        ensure!(
            self.from_revision_id == anchor.captured_revision_id
                && self.to_revision_id == current_revision_id,
            "invalid anchor continuity endpoints"
        );
        match self.state {
            ContinuityState::Unknown => ensure!(
                self.evidence.is_none(),
                "unknown anchor continuity cannot contain evidence"
            ),
            ContinuityState::Unchanged if anchor.captured_revision_id != current_revision_id => {
                ensure!(
                    self.evidence
                        .as_ref()
                        .is_some_and(|value| !value.trim().is_empty()),
                    "cross-revision unchanged continuity requires independent evidence"
                )
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn audit_anchor(
    anchor: &DurableAnchor,
    current_revision_id: &str,
    current: Option<&Declaration>,
    declarations: &[Declaration],
    continuity: Option<&GroupContinuity>,
) -> Result<AnchorResult> {
    anchor.validate()?;
    ensure!(
        !current_revision_id.is_empty(),
        "invalid current anchor revision"
    );
    let default_continuity = GroupContinuity {
        from_revision_id: anchor.captured_revision_id.clone(),
        to_revision_id: current_revision_id.to_owned(),
        state: if anchor.captured_revision_id == current_revision_id {
            ContinuityState::Unchanged
        } else {
            ContinuityState::Unknown
        },
        evidence: None,
    };
    let continuity = continuity.unwrap_or(&default_continuity);
    continuity.validate(anchor, current_revision_id)?;
    let Some(current) = current else {
        return Ok(AnchorResult::orphaned(AnchorReason::Missing));
    };
    ensure!(
        current.syntax_id == anchor.syntax_id,
        "invalid current anchor identity"
    );
    ensure!(
        current.document == anchor.document && current.revision_id == current_revision_id,
        "invalid current anchor document/revision"
    );
    if header_hash(&current.header)? != anchor.header_hash {
        return Ok(AnchorResult::orphaned(AnchorReason::HeaderMismatch));
    }
    let group = measured_group(current, declarations)?;
    ensure!(
        group
            .iter()
            .all(|row| row.document == anchor.document && row.revision_id == current_revision_id),
        "invalid current anchor group association"
    );
    let headers = group
        .iter()
        .map(|row| header_hash(&row.header))
        .collect::<Result<Vec<_>>>()?;
    let identical = headers
        .iter()
        .filter(|hash| **hash == anchor.header_hash)
        .count();
    if anchor.identical_header_count > 1 || identical > 1 {
        if sibling_group_hash(&headers)? != anchor.sibling_group_hash
            || headers.len() != anchor.sibling_count
            || identical != anchor.identical_header_count
            || (anchor.captured_revision_id != current_revision_id
                && continuity.state == ContinuityState::Changed)
        {
            return Ok(AnchorResult::orphaned(AnchorReason::GroupChanged));
        }
        if anchor.captured_revision_id != current_revision_id
            && continuity.state != ContinuityState::Unchanged
        {
            return Ok(AnchorResult::orphaned(AnchorReason::UnprovenContinuity));
        }
    }
    Ok(AnchorResult::attached(anchor.syntax_id.clone()))
}
