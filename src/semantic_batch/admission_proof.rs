//! The proof that batch admission ran. The storage-owned `AdmittedBatch` is built from one.

/// Evidence that the admission code in `crate::semantic_batch` completed its passes over a
/// batch. `crate::store::batch_input::AdmittedBatch::new` consumes one, so storage defines the
/// admitted batch and still cannot build it, and neither can any other code outside admission.
///
/// The compiler enforces that: the field is private to this module and `admitted`, the only
/// constructor, is `pub(super)`, so a value can be created only in `crate::semantic_batch` and
/// its descendants. That is the code that could call `AdmittedBatch::new` while it was private
/// to `semantic_batch`. There is no `Default`, `Clone` or `Copy`: one proof builds one batch.
pub(crate) struct AdmissionProof(());

impl AdmissionProof {
    /// Call it only where admission has finished, in the expression that builds the batch.
    pub(super) fn admitted() -> Self {
        Self(())
    }
}
