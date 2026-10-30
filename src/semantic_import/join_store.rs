//! Storage-neutral reads and writes behind the exact-join and rejoin algorithms.
//!
//! The binding, ambiguity and rejoin rules in `semantic_import` are written once against
//! these traits. A backend decides where claims, coordinate maps, measured native facts,
//! role sites, global indexes and output rows live; the algorithm never touches those
//! structures directly.
use super::{RoleSites, ScopedKey};
use crate::{
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::BTreeSet, ops::Deref};

/// Source bytes borrowed from the store's document owner.
pub(super) enum Bytes<'a> {
    Borrowed(&'a [u8]),
}

impl Deref for Bytes<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Borrowed(bytes) => bytes,
        }
    }
}

/// Measured native facts of one revision that only the in-memory rejoin consumes.
pub(super) trait NativeFacts {
    fn document(&self, document: &native::DocumentKey) -> Option<&native::Document>;
    fn source(&self, document: &native::DocumentKey) -> Option<&str>;
    /// Source bytes of one document, served on demand.
    fn source_bytes(&self, document: &native::DocumentKey) -> Option<&[u8]>;
    /// Documents that have source bytes, in key order.
    fn source_documents(&self) -> Box<dyn Iterator<Item = &native::DocumentKey> + '_>;
    fn declarations_by_id(&self, id: &str) -> &[&native::Declaration];
}

/// The measured native facts the exact join reads. Every lookup is scoped to one document and
/// returns owned or `Cow` data, so a backend may serve it from a bounded cache.
pub(super) trait JoinFacts {
    type Operation;
    fn operation(&self) -> Result<Self::Operation>;
    /// Admits `bytes()` into `operation`. The estimate is lazy: a backend without a live
    /// budget never evaluates it, so its overflow checks cannot fail the unmetered oracle.
    fn reserve_operation(
        &self,
        operation: &mut Self::Operation,
        bytes: impl FnOnce() -> Result<u64>,
    ) -> Result<()>;
    fn join_reserved(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
        _operation: &mut Self::Operation,
    ) -> Result<semantic::Join> {
        self.join(document, range, kind)
    }
    fn revision_id(&self) -> &str;
    fn source_set_id(&self) -> &str;
    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join>;
    /// Source bytes of one document.
    fn bytes_of(&self, document: &native::DocumentKey) -> Result<Option<Bytes<'_>>>;
    /// Documents of the revision that have source bytes, in key order.
    fn documents(&self) -> Box<dyn Iterator<Item = Result<native::DocumentKey>> + '_>;
    /// Every measured declaration of one document.
    fn document_declarations(
        &self,
        document: &native::DocumentKey,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>>;
    fn declaration_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Declaration>>>;
    fn declarations_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>>;
    fn declarations_with_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Declaration>>>;
    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> Result<bool>;
    fn call_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Call>>>;
    fn calls_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Call>>>;
    fn reference_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Reference>>>;
    fn references_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        operation: &mut Self::Operation,
    ) -> Result<Vec<Cow<'_, native::Reference>>>;
}

/// The original identity of one raw proof.
pub(super) struct ProofRef {
    pub(super) language: String,
    pub(super) path: String,
    pub(super) record_kind: String,
    pub(super) fact_digest: String,
    pub(super) artifact_hash: String,
}

/// Proof-identity lookups over one claim set.
pub(super) trait ProofIdentities {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>>;
    /// Confirms `(proof_id, kind, digest)` names exactly one claim.
    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()>;
}

/// The captured semantic basis of each document.
pub(super) trait BasisFor {
    fn basis_for(&self, document: &native::DocumentKey) -> Result<Option<semantic::SemanticBasis>>;
}

/// One occurrence claim with its pass-1 joins, kept for the binding pass.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct OccurrenceRow {
    pub(super) proof_id: String,
    pub(super) fact_digest: String,
    pub(super) document: native::DocumentKey,
    pub(super) symbol: String,
    pub(super) roles: u64,
    pub(super) binding: semantic::Join,
    pub(super) reference: semantic::Join,
    pub(super) call: semantic::Join,
}

/// One relationship claim, reduced to what the type-relationship pass reads.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct RelationshipRow {
    pub(super) proof_id: String,
    pub(super) source_symbol: Option<String>,
    pub(super) target_symbol: Option<String>,
}

/// A measured method and the exact foreign-owner predicate needed by Java overrides.
#[derive(Clone, Serialize, Deserialize)]
pub(super) struct MethodSummary {
    pub(super) method: native::Declaration,
    pub(super) target_owner_matches: bool,
}

impl Deref for MethodSummary {
    type Target = native::Declaration;
    fn deref(&self) -> &Self::Target {
        &self.method
    }
}

/// Admits the bytes of each returned row before it is built. A backend without a live budget
/// never calls it.
pub(super) type RowReserve<'a> = &'a mut dyn FnMut(u64) -> Result<()>;

/// Complete declaration, presence and method lookups for one claim set. Each lookup is one
/// pass over its rows: a metered backend admits a row's bytes through `reserve` before it
/// builds the row, so no separate sizing query precedes the fetch.
pub(super) trait GlobalJoinReads {
    fn declarations(
        &self,
        key: &ScopedKey,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<(native::DocumentKey, String)>>;
    /// `(declared, information)` presence of one symbol: in the Cargo target whose crate
    /// root is `target`, or, for `None`, in any document (#394).
    fn presence(&self, symbol: &str, target: Option<&native::DocumentKey>) -> Result<(bool, bool)>;
    fn is_information_symbol(&self, symbol: &str) -> Result<bool> {
        Ok(self.presence(symbol, None)?.1)
    }
    /// The Cargo targets in which `symbol` is present, as `(crate root, is library)` (#394).
    fn defining_targets(&self, symbol: &str) -> Result<Vec<ClaimTarget>>;
    fn source_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>>;
    fn target_methods(
        &self,
        document: &native::DocumentKey,
        symbol: &str,
        reserve: RowReserve<'_>,
    ) -> Result<Vec<MethodSummary>>;
}

pub(super) type ReferenceClaims = Vec<(String, semantic::Target)>;

/// Scratch state for occurrence and relationship classification. No global discovery writes.
pub(super) trait DocumentJoinState: GlobalJoinReads {
    type ClaimReservation;
    fn type_uses_clone_bytes(
        &self,
        _document: &native::DocumentKey,
        _start: usize,
        _end: usize,
    ) -> Result<u64> {
        Ok(0)
    }
    fn relationships_clone_bytes(&self, _language: &str, _path: &str) -> Result<u64> {
        Ok(0)
    }
    fn relationship_endpoints_clone_bytes(
        &self,
        _language: &str,
        _path: &str,
        _source: &str,
        _target: &str,
    ) -> Result<u64> {
        Ok(0)
    }
    fn call_reference_clone_bytes(&self, _proof_id: &str) -> Result<u64> {
        Ok(0)
    }
    fn put_type_use(
        &mut self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        symbol: &str,
    ) -> Result<()>;
    fn type_uses(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<BTreeSet<String>>;
    fn put_occurrence(&mut self, row: OccurrenceRow) -> Result<()>;
    fn occurrence_count(&self) -> usize;
    /// Logical clone bytes. Memory is unmetered; decoded staged rows override this.
    fn occurrence_clone_bytes(&self, _ordinal: usize) -> Result<u64> {
        Ok(0)
    }
    fn occurrence(&self, ordinal: usize) -> Result<OccurrenceRow>;
    fn put_relationship(&mut self, language: &str, path: &str, row: RelationshipRow) -> Result<()>;
    fn relationships(&self, language: &str, path: &str) -> Result<Vec<RelationshipRow>>;
    fn put_relationship_endpoint(
        &mut self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
        row: &RelationshipRow,
    ) -> Result<()>;
    fn relationship_endpoints(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<Vec<RelationshipRow>>;
    fn put_reference_claim(
        &mut self,
        reference_id: &str,
        proof_id: &str,
        target: semantic::Target,
    ) -> Result<()>;
    fn take_reference_claims(
        &mut self,
        reference_id: &str,
    ) -> Result<(ReferenceClaims, Self::ClaimReservation)>;
    fn put_call_reference(&mut self, proof_id: &str, reference_id: &str) -> Result<()>;
    fn call_reference(&self, proof_id: &str) -> Result<Option<String>>;
}

/// Whole-claim-set discovery used by the existing indexed driver.
pub(super) trait JoinIndex: DocumentJoinState {
    fn put_declaration(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
    ) -> Result<()>;
    fn put_symbol(
        &mut self,
        symbol: &str,
        target: Option<&ClaimTarget>,
        declared: bool,
        information: bool,
    ) -> Result<()>;
    fn put_method(
        &mut self,
        document: &native::DocumentKey,
        symbol: &str,
        method: &MethodSummary,
    ) -> Result<()>;
}

/// The Cargo target of a claim's document: its crate root and whether it is a library (#394).
pub(super) type ClaimTarget = (native::DocumentKey, bool);

/// Global information learned from authenticated claims, independent of occurrence storage.
pub(super) trait GlobalDiscoverySink {
    fn symbol_presence(
        &mut self,
        symbol: &str,
        target: Option<&ClaimTarget>,
        declared: bool,
        information: bool,
    ) -> Result<()>;
    fn exact_definition(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
        common_compatible: bool,
    ) -> Result<()>;
}

impl<I: JoinIndex> GlobalDiscoverySink for I {
    fn symbol_presence(
        &mut self,
        symbol: &str,
        target: Option<&ClaimTarget>,
        declared: bool,
        information: bool,
    ) -> Result<()> {
        self.put_symbol(symbol, target, declared, information)
    }

    fn exact_definition(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
        common_compatible: bool,
    ) -> Result<()> {
        if common_compatible {
            self.put_declaration(key, document, syntax_id)?;
        }
        Ok(())
    }
}

/// Rows produced by the exact join. Rows that later passes revise are addressed by slot.
pub(super) trait JoinSink {
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()>;
    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()>;
    fn join(&mut self, proof_id: &str, join: &semantic::Join) -> Result<()>;
    fn reference_claim(&mut self, proof_id: &str, row: semantic::Reference) -> Result<()>;
    fn reference_slot(&self, reference_id: &str) -> Result<Option<usize>>;
    fn reference_count(&self) -> usize;
    /// Logical clone bytes; zero for the unmetered in-memory sink.
    fn reference_clone_bytes(&self, _slot: usize) -> Result<u64> {
        Ok(0)
    }
    fn reference(&self, slot: usize) -> Result<semantic::Reference>;
    /// Replaces the row at `slot`, or appends when `slot` is `None`. Returns the slot.
    fn put_reference(&mut self, slot: Option<usize>, row: semantic::Reference) -> Result<usize>;
    fn call_binding_count(&self) -> usize;
    /// Logical clone bytes; zero for the unmetered in-memory sink.
    fn call_binding_clone_bytes(&self, _slot: usize) -> Result<u64> {
        Ok(0)
    }
    fn call_binding(&self, slot: usize) -> Result<semantic::CallBinding>;
    fn put_call_binding(
        &mut self,
        slot: Option<usize>,
        row: semantic::CallBinding,
    ) -> Result<usize>;
    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()>;
    fn diagnostic(&mut self, document: &native::DocumentKey, code: &str) -> Result<()>;
}

/// The final rows of a rejoin, emitted once every closure decision is made.
pub(super) trait PublicationSink {
    fn coverage(&mut self, row: semantic::Coverage) -> Result<()>;
    fn provenance(&mut self, row: semantic::Provenance) -> Result<()>;
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()>;
    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()>;
    fn reference(&mut self, row: semantic::Reference) -> Result<()>;
    fn call_binding(&mut self, row: semantic::CallBinding) -> Result<()>;
    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()>;
}

/// A claim set joined against one revision's measured facts.
pub(super) trait JoinStore: JoinFacts + ProofIdentities {
    /// The claim stream in canonical order.
    fn claims(&self) -> Box<dyn Iterator<Item = Result<Cow<'_, CapturedClaim>>> + '_>;
    /// Proof lookup by `(proof_id, kind, digest)`.
    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<()>;
    /// Re-reads a claim already admitted by `lookup_proof`.
    fn claim(&self, proof: &str, kind: &str, digest: &str) -> Result<Cow<'_, CapturedClaim>>;
    /// The SCIP kind of the symbol information for `symbol` in one document.
    fn symbol_kind(&self, language: &str, path: &str, symbol: &str) -> Result<Option<i64>>;
    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range>;
    fn enclosing_occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        let mut enclosing = claim.clone();
        enclosing.envelope.raw["range"] = claim.envelope.raw["enclosingRange"].clone();
        self.occurrence_range(&enclosing, admitted_encoding)
    }
    /// Role sites of one document, parsed on demand.
    fn role_sites(&self, document: &native::DocumentKey) -> Result<RoleSites>;
    fn role_sites_reserved(
        &self,
        document: &native::DocumentKey,
        _operation: &mut Self::Operation,
    ) -> Result<RoleSites> {
        self.role_sites(document)
    }
    /// The syntax tree role sites were read from for `document`, if the store kept it: the
    /// tree of the same source and grammar its direct type bases are read from. Taking it ends
    /// its lifetime; a store that keeps none parses again.
    fn take_parsed_tree(&self, _document: &native::DocumentKey) -> Option<tree_sitter::Tree> {
        None
    }
    /// Accounts `units` of lookup work against a backend budget.
    fn charge(&self, units: usize) -> Result<()>;
}

/// Whole-corpus aggregation retained only by the independent in-memory driver.
pub(super) trait IndexedJoinStore: JoinStore {
    /// The same coordinates and measured facts, served for a different claim set.
    type Restricted<'i>: IndexedJoinStore
    where
        Self: 'i;
    fn restrict<'i>(&'i self, claims: &'i CapturedScipInventory) -> Result<Self::Restricted<'i>>;
    type Index: JoinIndex;
    fn new_index(&self) -> Self::Index;
}
