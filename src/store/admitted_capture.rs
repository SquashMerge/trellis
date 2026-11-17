//! The admitted capture as publication sees it.
//!
//! Storage owns this trait and `capture::Capture`, in indexing, implements it by delegation.
//! Publication takes the trait, and so do the indexing functions publication hands its capture
//! to, so `store.rs` does not name the indexing type. Every method is the capture's own method
//! or field of the same name.
use crate::{
    capture_records::{CaptureInputObservation, CaptureStat, SourceOperations},
    model::{CancelFlag, ReconcileOptions, SourceFile},
};
use anyhow::Result;
use std::{collections::BTreeMap, path::Path};

/// One admitted set of filesystem inputs, as publication and the validation it calls read it.
pub trait AdmittedCapture {
    // Called by publication only.

    /// Fails when an admitted input changed since admission.
    fn verify(&self, cancel: &CancelFlag) -> Result<()>;
    /// How many graph projections have claimed this capture.
    fn graph_projection_count(&self) -> usize;
    /// Claims this capture's one graph projection.
    fn claim_graph_projection(&self) -> Result<()>;
    /// What admission recorded per source, by relative path.
    fn source_operations(&self) -> &BTreeMap<String, SourceOperations>;
    /// The admitted stat of one source, by relative path.
    fn source_stat(&self, relative: &str) -> Result<CaptureStat>;

    // Called by publication and by the indexing functions it hands the capture to.

    /// The admitted sources.
    fn files(&self) -> &[SourceFile];
    /// The running executable's path.
    fn executable_path(&self) -> &Path;
    /// The admitted digest of the executable at `path`.
    fn executable_digest(&self, path: &Path) -> Option<&str>;
    /// The reconcile options the capture was admitted under.
    fn reconcile_options(&self) -> &ReconcileOptions;
    /// The admitted root inputs in their persisted form.
    fn persisted_inputs(&self) -> Result<BTreeMap<String, CaptureInputObservation>>;

    // Read only by the indexing functions publication hands the capture to.

    /// The physical root the capture was admitted at.
    fn physical_root(&self) -> &Path;
    /// The admitted bytes of one input.
    fn bytes(&self, path: &Path) -> Option<&[u8]>;
    /// Every admitted input with its bytes, in the capture's order.
    fn admitted_inputs(&self) -> Box<dyn Iterator<Item = (&Path, Option<&[u8]>)> + '_>;
    /// The admitted content hash of each source, by relative path.
    fn hashes(&self) -> &BTreeMap<String, String>;
}
