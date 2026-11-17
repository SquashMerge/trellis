//! The per-document evidence budgets (#564) as plain numbers: what a selected read admits
//! and what extraction measures against, and the graph-row charge a selected read applies.
//! The charging of a document's native facts is in `evidence_budget`.

/// The coverage diagnostic of a document whose evidence was left out. Reads find such a
/// document by it, so a change here is a change of stored meaning.
pub(crate) const DIAGNOSTIC: &str =
    "native evidence exceeds the per-file budget; only the file module was kept";

/// Selected rows of one table, for a source of `bytes`.
pub(crate) fn row_limit(bytes: i64) -> i64 {
    bytes.saturating_mul(16).clamp(1024, 1_000_000)
}
/// Bytes of one native or class row.
pub(crate) fn max_row(bytes: i64) -> i64 {
    bytes
        .saturating_mul(32)
        .saturating_add(16 * 1024)
        .min(256 * 1024 * 1024)
}
/// Bytes of all of a document's native rows, of its class rows and of its graph rows, each.
/// A class takes members from other files, so class rows are only known when a revision is
/// composed, long after a document's native rows are measured. They have a budget of their own
/// so that neither gate depends on the other.
pub(crate) fn total(bytes: i64) -> i64 {
    bytes
        .saturating_mul(64)
        .saturating_add(256 * 1024)
        .min(512 * 1024 * 1024)
}
/// Bytes of the class extraction stored for one document, whatever its source.
pub(crate) const CLASS_EXTRACTION: usize = 64 * 1024 * 1024;
/// The former graph-row SQL charge: key text bytes plus the compact JSON payload.
pub(crate) fn graph_bytes(
    nodes: &[crate::model::Symbol],
    calls: &[crate::model::CallSite],
    regions: &[crate::model::ControlRegion],
    source_bytes: i64,
) -> anyhow::Result<u64> {
    fn encoded<T: serde::Serialize>(value: &T) -> anyhow::Result<usize> {
        Ok(serde_json::to_vec(value)?.len())
    }
    let mut largest = 0u64;
    let mut sum = 0u64;
    let mut add = |row: usize| {
        largest = largest.max(row as u64);
        sum = sum.saturating_add(row as u64);
    };
    for node in nodes {
        add(node.id.len() + node.name.len() + node.path.len() + encoded(node)?);
    }
    for call in calls {
        add(call.id.len() + call.caller.len() + call.path.len() + encoded(call)?);
    }
    for region in regions {
        add(region.id.len() + region.owner.len() + region.path.len() + encoded(region)?);
    }
    let bytes = source_bytes.max(0) as u64;
    anyhow::ensure!(
        largest
            <= bytes
                .saturating_mul(6)
                .saturating_add(16 * 1024)
                .min(256 * 1024 * 1024 + 16 * 1024)
            && sum <= total(source_bytes.max(0)) as u64,
        "incompatible_index: selected graph row byte budget exceeded"
    );
    Ok(sum)
}
