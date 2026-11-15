//! Reservations of the exact join, observed on the generic algorithm (#1129). The in-memory
//! store never meters, so these run it behind a store that records every reservation and can
//! run out at a chosen one, as the staged store's live budget does.
use super::super::{
    join_store::{Bytes, JoinFacts, ProofRef},
    memory::MemoryJoinIndex,
    *,
};
use super::case;
use crate::{capture::Capture, indexer::IndexOptions, scip_raw::ScipCaptureError};
use serde_json::json;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    fs,
    sync::{Arc, atomic::AtomicBool},
};

type Fixture = (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
);

/// The in-memory store with a metered budget in front of it.
struct Metered<'s, 'a> {
    inner: &'s MemoryJoinStore<'a>,
    operations: Cell<usize>,
    /// Every reservation in order, as `(operation, bytes)`.
    log: RefCell<Vec<(usize, u64)>>,
    /// The reservation, by its place in `log`, at which the budget runs out.
    deny: Option<usize>,
}

impl<'s, 'a> Metered<'s, 'a> {
    fn new(inner: &'s MemoryJoinStore<'a>, deny: Option<usize>) -> Self {
        Self {
            inner,
            operations: Cell::new(0),
            log: RefCell::default(),
            deny,
        }
    }
}

impl JoinFacts for Metered<'_, '_> {
    type Operation = usize;
    fn operation(&self) -> Result<usize> {
        let id = self.operations.get();
        self.operations.set(id + 1);
        Ok(id)
    }
    fn reserve_operation(
        &self,
        operation: &mut usize,
        bytes: impl FnOnce() -> Result<u64>,
    ) -> Result<()> {
        let bytes = bytes()?;
        let mut log = self.log.borrow_mut();
        let ordinal = log.len();
        log.push((*operation, bytes));
        if self.deny == Some(ordinal) {
            return Err(ScipCaptureError::LimitExceeded("unit expansion".into()).into());
        }
        Ok(())
    }
    fn revision_id(&self) -> &str {
        self.inner.revision_id()
    }
    fn source_set_id(&self) -> &str {
        self.inner.source_set_id()
    }
    fn join(
        &self,
        document: &native::DocumentKey,
        range: native::Range,
        kind: semantic::JoinKind,
    ) -> Result<semantic::Join> {
        self.inner.join(document, range, kind)
    }
    fn bytes_of(&self, document: &native::DocumentKey) -> Result<Option<Bytes<'_>>> {
        self.inner.bytes_of(document)
    }
    fn documents(&self) -> Box<dyn Iterator<Item = Result<native::DocumentKey>> + '_> {
        self.inner.documents()
    }
    fn document_declarations(
        &self,
        document: &native::DocumentKey,
        _operation: &mut usize,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        self.inner.document_declarations(document, &mut ())
    }
    fn declaration_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Declaration>>> {
        self.inner.declaration_of(document, id)
    }
    fn declarations_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        _operation: &mut usize,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        self.inner.declarations_with_id(document, id, &mut ())
    }
    fn declarations_with_key(
        &self,
        document: &native::DocumentKey,
        key: &[u8],
        _operation: &mut usize,
    ) -> Result<Vec<Cow<'_, native::Declaration>>> {
        self.inner.declarations_with_key(document, key, &mut ())
    }
    fn has_declaration_name(
        &self,
        document: &native::DocumentKey,
        start: usize,
        end: usize,
        spelling: &str,
    ) -> Result<bool> {
        self.inner
            .has_declaration_name(document, start, end, spelling)
    }
    fn call_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Call>>> {
        self.inner.call_of(document, id)
    }
    fn calls_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        _operation: &mut usize,
    ) -> Result<Vec<Cow<'_, native::Call>>> {
        self.inner.calls_with_id(document, id, &mut ())
    }
    fn reference_of(
        &self,
        document: &native::DocumentKey,
        id: &str,
    ) -> Result<Option<Cow<'_, native::Reference>>> {
        self.inner.reference_of(document, id)
    }
    fn references_with_id(
        &self,
        document: &native::DocumentKey,
        id: &str,
        _operation: &mut usize,
    ) -> Result<Vec<Cow<'_, native::Reference>>> {
        self.inner.references_with_id(document, id, &mut ())
    }
}

impl ProofIdentities for Metered<'_, '_> {
    fn proof(&self, proof_id: &str) -> Result<Option<ProofRef>> {
        self.inner.proof(proof_id)
    }
    fn check_proof(&self, proof_id: &str, kind: &str, digest: &str) -> Result<()> {
        self.inner.check_proof(proof_id, kind, digest)
    }
}

impl JoinStore for Metered<'_, '_> {
    fn claims(&self) -> Box<dyn Iterator<Item = Result<Cow<'_, CapturedClaim>>> + '_> {
        self.inner.claims()
    }
    fn lookup_proof(&self, proof: &str, kind: &str, digest: &str) -> Result<()> {
        self.inner.lookup_proof(proof, kind, digest)
    }
    fn claim(&self, proof: &str, kind: &str, digest: &str) -> Result<Cow<'_, CapturedClaim>> {
        self.inner.claim(proof, kind, digest)
    }
    fn has_unclaimed_source(&self, language: &str) -> Result<bool> {
        self.inner.has_unclaimed_source(language)
    }
    fn symbol_kind(&self, language: &str, path: &str, symbol: &str) -> Result<Option<i64>> {
        self.inner.symbol_kind(language, path, symbol)
    }
    fn occurrence_range(
        &self,
        claim: &CapturedClaim,
        admitted_encoding: u8,
    ) -> Result<native::Range> {
        self.inner.occurrence_range(claim, admitted_encoding)
    }
    fn role_sites(&self, document: &native::DocumentKey) -> Result<RoleSites> {
        self.inner.role_sites(document)
    }
    fn charge(&self, units: usize) -> Result<()> {
        self.inner.charge(units)
    }
}

impl<'a> IndexedJoinStore for Metered<'_, 'a> {
    type Restricted<'i>
        = <MemoryJoinStore<'a> as IndexedJoinStore>::Restricted<'i>
    where
        Self: 'i;
    fn restrict<'i>(&'i self, claims: &'i CapturedScipInventory) -> Result<Self::Restricted<'i>> {
        self.inner.restrict(claims)
    }
    type Index = MemoryJoinIndex;
    fn new_index(&self) -> MemoryJoinIndex {
        self.inner.new_index()
    }
}

fn with_store<T>(
    (_root, native, sources, inventory): &Fixture,
    encoding: u8,
    run: impl FnOnce(&MemoryJoinStore<'_>) -> T,
) -> T {
    let raw = InventoryIndex::new(inventory).unwrap();
    let coordinates = CoordinateMaps::new(inventory, native, sources, encoding).unwrap();
    let measured = MeasuredFacts::new(native, sources).unwrap();
    run(&MemoryJoinStore::new(
        &raw,
        &coordinates,
        &measured,
        sources,
    ))
}

fn assert_budget_denied(error: &anyhow::Error) {
    assert_eq!(
        error.downcast_ref::<ScipCaptureError>(),
        Some(&ScipCaptureError::LimitExceeded("unit expansion".into())),
        "{error:#}"
    );
}

/// One Java document: `A` implements `I`, and the enum `E` has only its implicit base.
fn java_fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    let source = "interface I {} class A implements I {} enum E { X }";
    fs::write(physical.join("A.java"), source).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let document = native.revision.documents[0].key.clone();
    let sources = BTreeMap::from([(document, source.as_bytes().to_vec())]);
    let (i, a, e) = ("scip-java pkg I#", "scip-java pkg A#", "scip-java pkg E#");
    let enum_base = "scip-java maven jdk 17 java/lang/Enum#";
    let occurrence = |at: usize, symbol: &str, roles: u64| {
        (
            "occurrence",
            json!({
                "range": {"encoding": "legacy", "values": [0, at, at + 1]},
                "enclosingRange": null, "symbol": symbol, "symbolRoles": roles, "syntaxKind": 0,
            }),
        )
    };
    let information =
        |symbol: &str, kind: u64| ("symbolInformation", json!({"symbol": symbol, "kind": kind}));
    let relationship = |source: &str, target: &str| {
        (
            "relationship",
            json!({"sourceSymbol": source, "targetSymbol": target}),
        )
    };
    let claims = [
        occurrence(source.find("I {").unwrap(), i, 1),
        occurrence(source.find("A ").unwrap(), a, 1),
        occurrence(source.rfind("I {").unwrap(), i, 8),
        occurrence(source.find("E {").unwrap(), e, 1),
        information(i, 21),
        information(a, 7),
        information(e, 11),
        relationship(a, i),
        relationship(e, enum_base),
    ]
    .into_iter()
    .enumerate()
    .map(|(ordinal, (kind, raw))| {
        let envelope = crate::scip_raw::CapturedScipFactV1 {
            format_version: 1,
            record_kind: kind.into(),
            relative_path: "A.java".into(),
            position_encoding: 1,
            raw,
            cargo_target: None,
        };
        let canonical_bytes = native_ids::canonical(&json!({
            "formatVersion": 1, "recordKind": kind, "relativePath": "A.java",
            "positionEncoding": 1, "raw": envelope.raw,
        }));
        CapturedClaim {
            fact_digest: native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes),
            captured_proof_id: native_ids::digest(
                b"reservation-fixture",
                ordinal.to_string().as_bytes(),
            ),
            canonical_bytes,
            envelope,
            language: "java".into(),
            artifact_hash: "0".repeat(64),
        }
    })
    .collect();
    let inventory = CapturedScipInventory {
        artifact_hashes: vec!["0".repeat(64)],
        decoder_version: crate::scip_raw::DECODER_VERSION.into(),
        source_manifest_hash: manifest_hash(&native.revision),
        completion: crate::scip_raw::Completion::Complete,
        exclusions: vec![],
        capped_documents: Default::default(),
        claims,
    };
    (root, native, sources, inventory)
}

/// `join_reference` admits each target of the occurrence before it records that target as a
/// claim on the measured reference: the target's bytes are the first thing reserved after the
/// role set. When the budget runs out there, `join_occurrence` fails typed with no claim
/// recorded and no reference row emitted.
#[test]
fn a_reference_target_is_reserved_before_it_is_claimed() {
    let fixture = case(1, false, 2);
    with_store(&fixture, 2, |memory| {
        let join = |deny| {
            let store = Metered::new(memory, deny);
            let mut index = build_join_index(2, memory).unwrap();
            let mut sink = MemoryJoinSink::default();
            let row = index.occurrence(0).unwrap();
            let mut operation = store.operation().unwrap();
            let result = join_occurrence(
                2,
                &store,
                &mut index,
                &mut sink,
                row,
                &mut None,
                &mut operation,
            );
            let own: Vec<(usize, u64)> = store
                .log
                .borrow()
                .iter()
                .enumerate()
                .filter(|(_, (owner, _))| *owner == operation)
                .map(|(ordinal, (_, bytes))| (ordinal, *bytes))
                .collect();
            (result, index, sink.into_facts(), own)
        };
        let (result, _, facts, own) = join(None);
        result.unwrap();
        let [reference] = facts.references.as_slice() else {
            panic!("one reference row: {:?}", facts.references);
        };
        let target = reference.declared_target.as_ref().expect("one target");
        let at = |bytes: u64| own.iter().position(|(_, own)| *own == bytes);
        let role_set = at(staged::sizes::map_entries::<semantic::Role, ()>(7).unwrap())
            .expect("the role set is reserved");
        let row = at(staged::sizes::reference(reference).unwrap())
            .expect("the reference row is reserved");
        assert!(role_set < row, "{own:?}");
        let (ordinal, bytes) = own[role_set + 1];
        assert_eq!(
            bytes,
            staged::sizes::target(target).unwrap(),
            "the target is reserved next: {own:?}"
        );

        let (result, mut index, facts, _) = join(Some(ordinal));
        assert_budget_denied(&result.unwrap_err());
        let (claims, ()) = index.take_reference_claims(&reference.id).unwrap();
        assert!(claims.is_empty(), "{claims:?}");
        assert!(facts.references.is_empty() && facts.reference_claims.is_empty());
        assert!(facts.call_bindings.is_empty());
    });
}

/// A document's relationship operation holds its direct bases, the Java base map with one
/// entry per owner, and the document's relationship rows (zero bytes from the in-memory
/// index). When the budget runs out at the base map, `emit_type_relationships` fails typed
/// with no relationship and no diagnostic emitted.
#[test]
fn a_java_document_reserves_its_base_map_before_any_relationship() {
    let fixture = java_fixture();
    with_store(&fixture, 1, |memory| {
        let relate = |deny| {
            let store = Metered::new(memory, deny);
            let mut index = build_join_index(1, memory).unwrap();
            discover_join_methods(memory, &mut index).unwrap();
            discover_relationship_claims(memory, &mut index).unwrap();
            let mut sink = MemoryJoinSink::default();
            let result = emit_type_relationships(&store, &index, &mut sink);
            (result, sink.into_facts(), store.log.into_inner())
        };
        let (result, facts, log) = relate(None);
        result.unwrap();
        let kinds: Vec<_> = facts.type_relationships.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                semantic::RelationshipKind::Implements,
                semantic::RelationshipKind::Extends
            ]
        );
        // The document's operation is the first the pass opens.
        let mut own: Vec<u64> = log
            .iter()
            .filter(|(owner, _)| *owner == 0)
            .map(|(_, bytes)| *bytes)
            .collect();
        let bases =
            (std::mem::size_of::<Vec<DirectBase>>() + std::mem::size_of::<DirectBase>()) as u64;
        let map = std::mem::size_of::<BTreeMap<(usize, usize), native::Range>>() as u64;
        let entry = staged::sizes::map_entries::<(usize, usize), native::Range>(1).unwrap();
        let mut expected = vec![bases, map, entry, 0];
        own.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            own, expected,
            "one direct base, the base map, its one owner and the relationship rows"
        );

        let denied = log.iter().position(|row| *row == (0, map)).unwrap();
        let (result, facts, _) = relate(Some(denied));
        assert_budget_denied(&result.unwrap_err());
        assert!(facts.type_relationships.is_empty(), "{facts:?}");
        assert!(facts.diagnostics.is_empty(), "{facts:?}");
    });
}

/// Whichever reservation the budget runs out at, in `join_occurrence`,
/// `emit_type_relationships` or any other step, the whole join fails typed and returns no
/// facts.
#[test]
fn a_join_denied_at_any_reservation_fails_typed_and_returns_no_facts() {
    let fixture = java_fixture();
    with_store(&fixture, 1, |memory| {
        let store = Metered::new(memory, None);
        let facts = join_into_facts(1, &store).unwrap();
        assert_eq!(facts.declaration_bindings.len(), 3);
        assert_eq!(facts.references.len(), 4);
        assert_eq!(facts.type_relationships.len(), 2);
        let reservations = store.log.borrow().len();
        for ordinal in 0..reservations {
            let denied = join_into_facts(1, &Metered::new(memory, Some(ordinal)));
            let error = denied
                .err()
                .unwrap_or_else(|| panic!("reservation {ordinal} denied, yet the join has facts"));
            assert_budget_denied(&error);
        }
    });
}
