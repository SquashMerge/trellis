//! In-memory `JoinStore` over fully materialized indexes.
use super::{
    CoordinateMaps, InventoryIndex, MeasuredFacts, RoleSites,
    join_store::{JoinStore, NativeFacts},
    source_role_sites,
};
use crate::{
    native_evidence as native,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::Result;

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
}

impl<'a> MemoryJoinStore<'a> {
    pub(super) fn new(
        raw: &'a InventoryIndex<'a>,
        coordinates: &'a CoordinateMaps,
        measured: &'a MeasuredFacts<'a>,
    ) -> Self {
        Self {
            raw: RawIndex::Borrowed(raw),
            coordinates,
            measured,
        }
    }
}

impl NativeFacts for MeasuredFacts<'_> {
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

impl<'a> JoinStore for MemoryJoinStore<'a> {
    type Restricted<'i>
        = MemoryJoinStore<'i>
    where
        Self: 'i;

    fn restrict<'i>(&'i self, claims: &'i CapturedScipInventory) -> Result<MemoryJoinStore<'i>> {
        Ok(MemoryJoinStore {
            raw: RawIndex::Owned(InventoryIndex::new(claims)?),
            coordinates: self.coordinates,
            measured: self.measured,
        })
    }

    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<&CapturedClaim> {
        self.raw.lookup(proof, kind, digest)
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

    fn role_sites(&self, document: &native::DocumentKey, source: &[u8]) -> Result<RoleSites> {
        #[cfg(feature = "test-support")]
        super::note_parsed_role_site_document(document);
        source_role_sites(document.language.as_str(), source)
    }

    fn charge(&self, _units: usize) -> Result<()> {
        Ok(())
    }
}
