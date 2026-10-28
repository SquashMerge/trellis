//! In-memory `JoinStore` over fully materialized indexes.
use super::{
    CoordinateMaps, InventoryIndex, JoinedFacts, MeasuredFacts, RoleSites, ScopedKey,
    join_store::{
        BasisFor, JoinIndex, JoinSink, JoinStore, NativeFacts, OccurrenceRow, ProofIdentities,
        ProofRef, PublicationSink, RelationshipRow,
    },
    source_role_sites,
};
use crate::{
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::{Context, Result};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

pub(super) enum RawIndex<'a> {
    Borrowed(&'a InventoryIndex<'a>),
    Owned(InventoryIndex<'a>),
}

impl<'a> std::ops::Deref for RawIndex<'a> {
    type Target = InventoryIndex<'a>;
    fn deref(&self) -> &Self::Target {
        match self {
            RawIndex::Borrowed(index) => index,
            RawIndex::Owned(index) => index,
        }
    }
}

pub(super) struct MemoryJoinStore<'a> {
    raw: RawIndex<'a>,
    coordinates: &'a CoordinateMaps,
    measured: &'a MeasuredFacts<'a>,
    sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
}

impl<'a> MemoryJoinStore<'a> {
    pub(super) fn new(
        raw: &'a InventoryIndex<'a>,
        coordinates: &'a CoordinateMaps,
        measured: &'a MeasuredFacts<'a>,
        sources: &'a BTreeMap<native::DocumentKey, Vec<u8>>,
    ) -> Self {
        Self {
            raw: RawIndex::Borrowed(raw),
            coordinates,
            measured,
            sources,
        }
    }
}

impl NativeFacts for MeasuredFacts<'_> {
    fn revision_id(&self) -> &str {
        &self.native.revision.id
    }

    fn source_set_id(&self) -> &str {
        &self.native.revision.source_set_id
    }

    fn source_bytes(&self, document: &native::DocumentKey) -> Option<&[u8]> {
        self.sources.get(document).map(|text| text.as_bytes())
    }

    fn source_documents(&self) -> Box<dyn Iterator<Item = &native::DocumentKey> + '_> {
        Box::new(self.sources.keys())
    }

    fn native_declarations(&self) -> Box<dyn Iterator<Item = &native::Declaration> + '_> {
        Box::new(self.native.declarations.iter())
    }

    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        MeasuredFacts::join(self, document, range, kind)
    }

    fn document(&self, document: &native::DocumentKey) -> Option<&native::Document> {
        self.documents.get(document).copied()
    }

    fn source(&self, document: &native::DocumentKey) -> Option<&str> {
        self.sources.get(document).copied()
    }

    fn declaration(&self, id: &str) -> Option<&native::Declaration> {
        MeasuredFacts::declaration(self, id)
    }

    fn declarations_by_id(&self, id: &str) -> &[&native::Declaration] {
        self.declarations.get(id).map_or(&[], Vec::as_slice)
    }

    fn declarations_by_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
    ) -> &[&native::Declaration] {
        self.declaration_keys
            .get(&(document.clone(), key.to_vec()))
            .map_or(&[], Vec::as_slice)
    }

    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> bool {
        self.declaration_names
            .contains_key(&(document.clone(), start, end, spelling.to_owned()))
    }

    fn call(&self, id: &str) -> Option<&native::Call> {
        MeasuredFacts::call(self, id)
    }

    fn calls_by_id(&self, id: &str) -> &[&native::Call] {
        self.calls.get(id).map_or(&[], Vec::as_slice)
    }

    fn reference(&self, id: &str) -> Option<&native::Reference> {
        MeasuredFacts::reference(self, id)
    }

    fn references_by_id(&self, id: &str) -> &[&native::Reference] {
        self.references.get(id).map_or(&[], Vec::as_slice)
    }
}

impl NativeFacts for MemoryJoinStore<'_> {
    fn revision_id(&self) -> &str {
        NativeFacts::revision_id(self.measured)
    }

    fn source_set_id(&self) -> &str {
        NativeFacts::source_set_id(self.measured)
    }

    fn source_bytes(&self, document: &native::DocumentKey) -> Option<&[u8]> {
        self.sources.get(document).map(Vec::as_slice)
    }

    fn source_documents(&self) -> Box<dyn Iterator<Item = &native::DocumentKey> + '_> {
        Box::new(self.sources.keys())
    }

    fn native_declarations(&self) -> Box<dyn Iterator<Item = &native::Declaration> + '_> {
        self.measured.native_declarations()
    }

    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        self.measured.join(document, range, kind)
    }

    fn document(&self, document: &native::DocumentKey) -> Option<&native::Document> {
        NativeFacts::document(self.measured, document)
    }

    fn source(&self, document: &native::DocumentKey) -> Option<&str> {
        NativeFacts::source(self.measured, document)
    }

    fn declaration(&self, id: &str) -> Option<&native::Declaration> {
        self.measured.declaration(id)
    }

    fn declarations_by_id(&self, id: &str) -> &[&native::Declaration] {
        self.measured.declarations_by_id(id)
    }

    fn declarations_by_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
    ) -> &[&native::Declaration] {
        self.measured.declarations_by_key(document, key)
    }

    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> bool {
        self.measured
            .has_declaration_name(document, start, end, spelling)
    }

    fn call(&self, id: &str) -> Option<&native::Call> {
        self.measured.call(id)
    }

    fn calls_by_id(&self, id: &str) -> &[&native::Call] {
        self.measured.calls_by_id(id)
    }

    fn reference(&self, id: &str) -> Option<&native::Reference> {
        self.measured.reference(id)
    }

    fn references_by_id(&self, id: &str) -> &[&native::Reference] {
        self.measured.references_by_id(id)
    }
}

impl ProofIdentities for MemoryJoinStore<'_> {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>> {
        Ok(self.raw.by_proof.get(proof_id).map(|claim| ProofRef {
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
            record_kind: claim.envelope.record_kind.clone(),
            fact_digest: claim.fact_digest.clone(),
            artifact_hash: claim.artifact_hash.clone(),
        }))
    }

    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()> {
        self.raw.resolve(proof_id, kind, digest).map(|_| ())
    }
}

impl<'a> JoinStore for MemoryJoinStore<'a> {
    type Restricted<'i>
        = MemoryJoinStore<'i>
    where
        Self: 'i;
    type Index = MemoryJoinIndex;

    fn restrict<'i>(&'i self, claims: &'i CapturedScipInventory) -> Result<MemoryJoinStore<'i>> {
        Ok(MemoryJoinStore {
            raw: RawIndex::Owned(InventoryIndex::new(claims)?),
            coordinates: self.coordinates,
            measured: self.measured,
            sources: self.sources,
        })
    }

    fn new_index(&self) -> MemoryJoinIndex {
        MemoryJoinIndex::default()
    }

    fn claims(&self) -> Box<dyn Iterator<Item = Result<Cow<'_, CapturedClaim>>> + '_> {
        Box::new(self.raw.claims.iter().map(|claim| Ok(Cow::Borrowed(claim))))
    }

    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<&CapturedClaim> {
        self.raw.lookup(proof, kind, digest)
    }

    fn claim(&self, proof: &str, kind: &str, digest: &str) -> Result<Cow<'_, CapturedClaim>> {
        self.raw.resolve(proof, kind, digest).map(Cow::Borrowed)
    }

    fn symbol_info(&self, language: &str, path: &str, symbol: &str) -> Option<&CapturedClaim> {
        self.raw
            .symbol_info
            .get(&(language.to_owned(), path.to_owned(), symbol.to_owned()))
            .copied()
    }

    fn is_declared_symbol(&self, symbol: &str) -> bool {
        self.raw.declared_symbols.contains(symbol)
    }

    fn is_information_symbol(&self, symbol: &str) -> bool {
        self.raw.information_symbols.contains(symbol)
    }

    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        self.coordinates.range(claim, admitted_encoding)
    }

    fn role_sites(&self, document: &native::DocumentKey) -> Result<RoleSites> {
        #[cfg(feature = "test-support")]
        super::note_parsed_role_site_document(document);
        let source = self
            .sources
            .get(document)
            .context("missing role-site source")?;
        source_role_sites(document.language.as_str(), source)
    }

    fn charge(&self, _units: usize) -> Result<()> {
        Ok(())
    }
}

/// Global join indexes held fully in memory.
#[derive(Default)]
pub(super) struct MemoryJoinIndex {
    declarations: BTreeMap<ScopedKey, BTreeSet<(native::DocumentKey, String)>>,
    type_uses: BTreeMap<(native::DocumentKey, usize, usize), BTreeSet<String>>,
    occurrences: Vec<OccurrenceRow>,
    relationships: BTreeMap<(String, String), Vec<RelationshipRow>>,
    relationship_endpoints: BTreeMap<(String, String, String, String), Vec<RelationshipRow>>,
    reference_claims: BTreeMap<String, Vec<(String, semantic::Target)>>,
    call_references: BTreeMap<String, String>,
}

impl JoinIndex for MemoryJoinIndex {
    fn put_declaration(
        &mut self,
        key: ScopedKey,
        document: native::DocumentKey,
        syntax_id: String,
    ) -> Result<()> {
        self.declarations
            .entry(key)
            .or_default()
            .insert((document, syntax_id));
        Ok(())
    }

    fn declarations(&self, key: &ScopedKey) -> Result<Vec<(native::DocumentKey, String)>> {
        Ok(self
            .declarations
            .get(key)
            .into_iter()
            .flatten()
            .cloned()
            .collect())
    }

    fn put_type_use(
        &mut self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        symbol: &str,
    ) -> Result<()> {
        self.type_uses
            .entry((document.clone(), start, end))
            .or_default()
            .insert(symbol.into());
        Ok(())
    }

    fn type_uses(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
    ) -> Result<BTreeSet<String>> {
        Ok(self
            .type_uses
            .get(&(document.clone(), start, end))
            .cloned()
            .unwrap_or_default())
    }

    fn put_occurrence(&mut self, row: OccurrenceRow) -> Result<()> {
        self.occurrences.push(row);
        Ok(())
    }

    fn occurrence_count(&self) -> usize {
        self.occurrences.len()
    }

    fn occurrence(&self, ordinal: usize) -> Result<OccurrenceRow> {
        self.occurrences
            .get(ordinal)
            .cloned()
            .context("missing occurrence row")
    }

    fn put_relationship(&mut self, language: &str, path: &str, row: RelationshipRow) -> Result<()> {
        self.relationships
            .entry((language.to_owned(), path.to_owned()))
            .or_default()
            .push(row);
        Ok(())
    }

    fn relationships(&self, language: &str, path: &str) -> Result<Vec<RelationshipRow>> {
        Ok(self
            .relationships
            .get(&(language.to_owned(), path.to_owned()))
            .cloned()
            .unwrap_or_default())
    }

    fn put_relationship_endpoint(
        &mut self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
        row: RelationshipRow,
    ) -> Result<()> {
        self.relationship_endpoints
            .entry((
                language.to_owned(),
                path.to_owned(),
                source.to_owned(),
                target.to_owned(),
            ))
            .or_default()
            .push(row);
        Ok(())
    }

    fn relationship_endpoints(
        &self,
        language: &str,
        path: &str,
        source: &str,
        target: &str,
    ) -> Result<Vec<RelationshipRow>> {
        Ok(self
            .relationship_endpoints
            .get(&(
                language.to_owned(),
                path.to_owned(),
                source.to_owned(),
                target.to_owned(),
            ))
            .cloned()
            .unwrap_or_default())
    }

    fn put_reference_claim(
        &mut self,
        reference_id: &str,
        proof_id: &str,
        target: semantic::Target,
    ) -> Result<()> {
        self.reference_claims
            .entry(reference_id.to_owned())
            .or_default()
            .push((proof_id.to_owned(), target));
        Ok(())
    }

    fn take_reference_claims(
        &mut self,
        reference_id: &str,
    ) -> Result<Vec<(String, semantic::Target)>> {
        Ok(self
            .reference_claims
            .remove(reference_id)
            .unwrap_or_default())
    }

    fn put_call_reference(&mut self, proof_id: &str, reference_id: &str) -> Result<()> {
        self.call_references
            .insert(proof_id.to_owned(), reference_id.to_owned());
        Ok(())
    }

    fn call_reference(&self, proof_id: &str) -> Result<Option<String>> {
        Ok(self.call_references.get(proof_id).cloned())
    }
}

/// `JoinedFacts` as the in-memory join sink.
#[derive(Default)]
pub(super) struct MemoryJoinSink {
    facts: JoinedFacts,
    reference_indices: BTreeMap<String, usize>,
}

impl MemoryJoinSink {
    pub(super) fn into_facts(self) -> JoinedFacts {
        self.facts
    }
}

impl JoinSink for MemoryJoinSink {
    fn symbol(&mut self, row: semantic::Symbol) -> Result<()> {
        self.facts.symbols.push(row);
        Ok(())
    }

    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()> {
        self.facts.declaration_bindings.push(row);
        Ok(())
    }

    fn join(&mut self, proof_id: &str, join: semantic::Join) -> Result<()> {
        self.facts.joins.push((proof_id.to_owned(), join));
        Ok(())
    }

    fn reference_claim(&mut self, proof_id: &str, row: semantic::Reference) -> Result<()> {
        self.facts.reference_claims.insert(proof_id.to_owned(), row);
        Ok(())
    }

    fn reference_slot(&self, reference_id: &str) -> Option<usize> {
        self.reference_indices.get(reference_id).copied()
    }

    fn reference_count(&self) -> usize {
        self.facts.references.len()
    }

    fn reference(&self, slot: usize) -> Result<semantic::Reference> {
        self.facts
            .references
            .get(slot)
            .cloned()
            .context("missing reference row")
    }

    fn put_reference(&mut self, slot: Option<usize>, row: semantic::Reference) -> Result<usize> {
        match slot {
            Some(slot) => {
                *self
                    .facts
                    .references
                    .get_mut(slot)
                    .context("missing reference row")? = row;
                Ok(slot)
            }
            None => {
                let slot = self.facts.references.len();
                self.reference_indices.insert(row.id.clone(), slot);
                self.facts.references.push(row);
                Ok(slot)
            }
        }
    }

    fn call_binding_count(&self) -> usize {
        self.facts.call_bindings.len()
    }

    fn call_binding(&self, slot: usize) -> Result<semantic::CallBinding> {
        self.facts
            .call_bindings
            .get(slot)
            .cloned()
            .context("missing call binding row")
    }

    fn put_call_binding(
        &mut self,
        slot: Option<usize>,
        row: semantic::CallBinding,
    ) -> Result<usize> {
        match slot {
            Some(slot) => {
                *self
                    .facts
                    .call_bindings
                    .get_mut(slot)
                    .context("missing call binding row")? = row;
                Ok(slot)
            }
            None => {
                self.facts.call_bindings.push(row);
                Ok(self.facts.call_bindings.len() - 1)
            }
        }
    }

    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()> {
        self.facts.type_relationships.push(row);
        Ok(())
    }

    fn diagnostic(&mut self, document: &native::DocumentKey, code: &str) -> Result<()> {
        self.facts
            .diagnostics
            .entry(document.clone())
            .or_default()
            .insert(code.into());
        Ok(())
    }
}

impl PublicationSink for semantic::SemanticPublication {
    fn coverage(&mut self, row: semantic::Coverage) -> Result<()> {
        self.coverage.push(row);
        Ok(())
    }

    fn provenance(&mut self, row: semantic::Provenance) -> Result<()> {
        self.provenance.push(row);
        Ok(())
    }

    fn symbol(&mut self, row: semantic::Symbol) -> Result<()> {
        self.symbols.push(row);
        Ok(())
    }

    fn declaration_binding(&mut self, row: semantic::DeclarationBinding) -> Result<()> {
        self.declaration_bindings.push(row);
        Ok(())
    }

    fn reference(&mut self, row: semantic::Reference) -> Result<()> {
        self.references.push(row);
        Ok(())
    }

    fn call_binding(&mut self, row: semantic::CallBinding) -> Result<()> {
        self.call_bindings.push(row);
        Ok(())
    }

    fn type_relationship(&mut self, row: semantic::TypeRelationship) -> Result<()> {
        self.type_relationships.push(row);
        Ok(())
    }
}

impl BasisFor for BTreeMap<native::DocumentKey, semantic::SemanticBasis> {
    fn basis_for(&self, document: &native::DocumentKey) -> Result<Option<semantic::SemanticBasis>> {
        Ok(self.get(document).cloned())
    }
}
