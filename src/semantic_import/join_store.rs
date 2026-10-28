//! Storage-neutral reads behind the exact-join and rejoin algorithms.
//!
//! The binding, ambiguity and rejoin rules in `semantic_import` are written once against
//! these traits. A backend decides where claims, coordinate maps, measured native facts and
//! role sites live; the algorithm never touches those structures directly.
use super::RoleSites;
use crate::{
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::Result;

/// Measured native facts for one revision.
pub(super) trait NativeFacts {
    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join>;
    fn document(&self, document: &native::DocumentKey) -> Option<&native::Document>;
    fn source(&self, document: &native::DocumentKey) -> Option<&str>;
    fn declaration(&self, id: &str) -> Option<&native::Declaration>;
    fn declarations_by_id(&self, id: &str) -> &[&native::Declaration];
    fn declarations_by_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
    ) -> &[&native::Declaration];
    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> bool;
    fn call(&self, id: &str) -> Option<&native::Call>;
    fn calls_by_id(&self, id: &str) -> &[&native::Call];
    fn reference(&self, id: &str) -> Option<&native::Reference>;
    fn references_by_id(&self, id: &str) -> &[&native::Reference];
}

/// A claim set joined against one revision's measured facts.
pub(super) trait JoinStore: NativeFacts {
    /// The same coordinates and measured facts, served for a different claim set.
    type Restricted<'i>: JoinStore
    where
        Self: 'i;

    fn restrict<'i>(&'i self, claims: &'i CapturedScipInventory) -> Result<Self::Restricted<'i>>;
    /// Proof lookup by `(proof_id, kind, digest)`.
    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<&CapturedClaim>;
    fn symbol_info(&self, language: &str, path: &str, symbol: &str) -> Option<&CapturedClaim>;
    fn is_declared_symbol(&self, symbol: &str) -> bool;
    fn is_information_symbol(&self, symbol: &str) -> bool;
    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range>;
    fn role_sites(&self, document: &native::DocumentKey, source: &[u8]) -> Result<RoleSites>;
    /// Accounts `units` of lookup work against a backend budget.
    fn charge(&self, units: usize) -> Result<()>;
}
