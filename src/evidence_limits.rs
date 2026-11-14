//! The per-document evidence budgets (#564) as plain numbers: what a selected read admits
//! and what extraction measures against. The charging of a document's facts is in
//! `evidence_budget`.

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
