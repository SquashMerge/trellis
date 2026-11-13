use super::*;
use crate::store::mcp_read::{
    McpDeclaration, McpDeclarationWithLines, McpDocumentVersion, McpIncomingCallWithLines,
    McpIncomingWithLines, McpItemSource, McpLines,
};

const SOURCE_SET: &str = "source-set:v1:0d7bc85f-6ff5-4e8f-b71a-8a2a5a60a4ab";
const REVISION: &str =
    "revision:v2:5b541324769de8538eabba403d11f5f8fcfbf8dd27ef2e07911857b328ccb4c5";

fn hex(seed: usize, len: usize) -> String {
    format!("{seed:x}").repeat(len)[..len].to_owned()
}

fn document(path: &str) -> Value {
    json!({"language":"rust","path":path,"sourceSetId":SOURCE_SET})
}

/// The basis rust-analyzer imports carry, as the live install serialized it.
fn basis() -> Value {
    json!({"artifactHash":hex(1, 64),"configHash":hex(2, 64),"dependencyHash":hex(3, 64),
        "language":"rust","lookupDependencies":[],"producerHash":hex(4, 64),
        "producerId":"rust-analyzer","producerVersion":"1.98.1 (48a229ce 2026-09-01)",
        "revisionId":REVISION,"sourceManifestHash":hex(5, 64),"sourceSetId":SOURCE_SET,
        "toolchainHash":hex(6, 64)})
}

fn native_coverage(path: &str) -> crate::native_evidence::Coverage {
    serde_json::from_value(json!({"producerId":"trellis.native.syntax","language":"rust",
        "sourceSetId":SOURCE_SET,"documentPath":path,"revisionId":REVISION,"requested":true,
        "selected":true,"state":"partial","supportedRoles":["definition","call"],
        "observedRoles":["definition","call"],
        "diagnostic":"opaque Rust macro/attribute token trees skipped; calls and definitions inside were not measured"}))
    .unwrap()
}

fn native_proof(path: &str, file: usize) -> crate::native_evidence::Provenance {
    serde_json::from_value(
        json!({"id":format!("native-proof:v1:{}", hex(file + 7, 64)),
        "producerId":"trellis.native.syntax","document":document(path),"revisionId":REVISION,
        "contentHash":hex(file + 7, 64),"evidenceKind":"measuredSyntax","basis":null,
        "freshness":"fresh","derivedFrom":null}),
    )
    .unwrap()
}

fn declaration(
    path: &str,
    file: usize,
    name: &str,
    start: usize,
) -> crate::native_evidence::Declaration {
    serde_json::from_value(json!({"syntaxId":format!("sid:v1:{}", hex(start + 9, 32)),
        "document":document(path),"revisionId":REVISION,"kind":"function","role":"test",
        "name":name,"lookupKey":name,"ancestors":[],
        "key":{"kind":"function","name":name,"signature":null,"ordinal":0},
        "range":{"start":start,"end":start + 900},"nameRange":null,
        "header":{"kind":"function","name":name,"modifiers":[],"typeParameters":[],
            "parameters":[],"resultType":null,"bases":[]},
        "provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
    .unwrap()
}

fn version(path: &str, file: usize) -> McpDocumentVersion {
    McpDocumentVersion {
        document: serde_json::from_value(document(path)).unwrap(),
        revision_id: REVISION.into(),
        content_hash: hex(file + 7, 64),
        provenance_id: format!("native-proof:v1:{}", hex(file + 7, 64)),
    }
}

/// `callers` name-matched sites, two per test file, each with a resolved
/// rust-analyzer binding and its own proof, as `index_workspace`'s callers have.
fn incoming(callers: usize) -> (McpIncomingWithLines, crate::store::McpSemantic) {
    let owner_path = "src/indexer.rs";
    let owner = declaration(owner_path, 0, "index_workspace", 100);
    let selected = McpDeclarationWithLines {
        selected: McpDeclaration {
            declaration: owner.clone(),
            content_hash: hex(7, 64),
            coverage: vec![native_coverage(owner_path)],
            provenance: vec![native_proof(owner_path, 0)],
            pin: serde_json::from_value(json!({
                "indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":6}))
            .unwrap(),
        },
        source: McpItemSource {
            stable_lookup_key: None,
            document: version(owner_path, 0),
            lines: Some(McpLines { start: 10, end: 40 }),
        },
    };
    let mut calls = Vec::new();
    let mut semantic = Vec::new();
    for i in 0..callers {
        let file = 1 + i / 2; // two callers per file, as index_workspace has
        let path = format!("tests/index/index_workspace_{file}.rs");
        let start = 1000 * (i + 1);
        let caller = declaration(&path, file, &format!("indexes_case_{i}"), start);
        let call_id = format!("occ:v2:{}", hex(i + 3, 32));
        let call: crate::native_evidence::Call = serde_json::from_value(json!({
            "id":call_id,"ownerSyntaxId":caller.syntax_id,"ordinal":0,
            "document":document(&path),"revisionId":REVISION,
            "range":{"start":start + 40,"end":start + 80},
            "calleeRange":{"start":start + 40,"end":start + 55},"spelling":"index_workspace",
            "regionIds":[],"provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
        .unwrap();
        let proof_id = hex(i + 11, 64);
        let proof = json!({"id":proof_id,"producerId":"rust-analyzer",
            "document":document(&path),"revisionId":REVISION,"contentHash":hex(file + 7, 64),
            "evidenceKind":"semanticReference","basis":basis(),"freshness":"fresh",
            "derivedFrom":{"factDigest":hex(i + 12, 64),"provenanceId":hex(i + 13, 64),
                "recordKind":"occurrence"}});
        let binding = json!({"callId":call_id,"candidates":[],
            "declaredTarget":{"document":document(owner_path),"kind":"internal",
                "revisionId":REVISION,"syntaxId":owner.syntax_id},
            "dispatch":"unknown",
            "join":{"anchor":{"contentHash":hex(file + 7, 64),"document":document(&path),
                "kind":"callee","range":{"start":start + 40,"end":start + 55},
                "revisionId":REVISION},"candidateIds":[call_id],"diagnostic":null,
                "status":"exact"},
            "possibleDispatch":[],"possibleDispatchComplete":false,"provenanceId":proof_id,
            "resolution":"resolved","staleTarget":false});
        let coverage = json!({"producerId":"rust-analyzer","language":"rust",
            "sourceSetId":SOURCE_SET,"documentPath":path,"revisionId":REVISION,
            "requested":true,"selected":true,"state":"partial",
            "supportedRoles":["definition","read","write","call","type","import","alias"],
            "observedRoles":["definition","read","call","type"],
            "diagnostic":"unsupported_native_gap"});
        semantic.push(Some(crate::store::CallSemantic {
            projection: None,
            historical_freshness: crate::semantic_evidence::Freshness::Fresh,
            effective_freshness: crate::semantic_evidence::Freshness::Fresh,
            binding: serde_json::from_value(binding).unwrap(),
            provenance: serde_json::from_value(proof.clone()).unwrap(),
            boundary_reason: crate::graph_query::BoundaryReason::Dispatch,
            target_id: Some(owner.syntax_id.clone()),
            coverage: serde_json::from_value(coverage).unwrap(),
            proofs: vec![serde_json::from_value(proof).unwrap()],
        }));
        calls.push(McpIncomingCallWithLines {
            call,
            caller,
            content_hash: hex(file + 7, 64),
            source: McpItemSource {
                stable_lookup_key: None,
                document: version(&path, file),
                lines: Some(McpLines {
                    start: 40 + i as u64,
                    end: 41 + i as u64,
                }),
            },
            callee_text: "index_workspace".into(),
            coverage: native_coverage(&path),
            provenance: native_proof(&path, file),
        });
    }
    (
        McpIncomingWithLines {
            selected,
            calls,
            same_name_declarations: 1,
            scope: [[0; 5], [355, 22, 333, 0, 0], [0; 5], [0; 5], [0; 5]],
            has_more: true,
            bound_elsewhere_omitted: 0,
        },
        crate::store::McpSemantic {
            documents: vec![],
            producer_id: Some("rust-analyzer".into()),
            note: None,
            calls: semantic,
        },
    )
}

fn frame_bytes(envelope: &Value) -> usize {
    let frame = json!({"jsonrpc":"2.0","id":1,
        "result":crate::mcp::tools::result("trellis_incoming_calls", envelope.clone(), true)});
    serde_json::to_vec(&frame).unwrap().len() + 1
}

fn project(callers: usize, compact: bool) -> Value {
    let (result, semantic) = incoming(callers);
    super::incoming(
        &json!(1),
        result,
        &semantic,
        &super::test_support::rust_semantic("fresh", [326, 14, 312, 0, 29]),
        compact,
        std::path::Path::new("/Users/someone/projects/trellis"),
        false,
        true,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn default_semantic_caller_page_stays_under_25_kib() {
    let page = project(crate::mcp::CALL_DEFAULT_LIMIT, false);
    assert_eq!(
        page["data"]["callers"].as_array().unwrap().len(),
        crate::mcp::CALL_DEFAULT_LIMIT
    );
    let bytes = frame_bytes(&page);
    assert!(bytes < 25 * 1024, "default incoming page is {bytes} bytes");
    // The basis is emitted once and referenced from every caller and proof.
    let bases = page["data"]["semanticBases"].as_array().unwrap();
    assert_eq!(bases.len(), 1);
    assert_eq!(bases[0]["ref"], 0);
    assert_eq!(bases[0]["producerHash"], hex(4, 64));
    let wire = serde_json::to_string(&page).unwrap();
    assert_eq!(wire.matches(&hex(4, 64)).count(), 1);
    for caller in page["data"]["callers"].as_array().unwrap() {
        assert_eq!(caller["semanticBasisRef"], 0);
        assert_eq!(caller["evidenceTier"], "semantic");
        assert!(caller.get("semanticBasis").is_none());
    }
    for proof in page["data"]["provenance"].as_array().unwrap() {
        assert!(proof.get("basis").is_none());
        let expected = if proof["evidenceKind"] == "measuredSyntax" {
            json!(null)
        } else {
            json!(0)
        };
        assert_eq!(proof["basisRef"], expected);
    }
    // Rust has fresh imported evidence: no syntaxOnly, and scope reports it beside
    // native coverage, which stays partial.
    assert!(
        page["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["code"] != "syntaxOnly")
    );
    let rust = &page["data"]["scopeCoverage"][1];
    assert_eq!(rust["nativeCoverageTier"], "syntax");
    assert_eq!(rust["coverage"]["partial"], 333);
    assert_eq!(rust["semantic"]["available"], true);
    assert_eq!(rust["semantic"]["coverage"]["partial"], 312);
    assert_eq!(page["partial"], true);
}

#[test]
fn the_basis_table_keeps_the_emitted_caller_prefix_inside_the_response_cap() {
    use std::collections::{BTreeMap, BTreeSet};
    let requested = 20;
    let page = project(requested, false);
    let callers = page["data"]["callers"].as_array().unwrap();
    let emitted = callers.len();
    assert!(emitted > 0 && emitted < requested);
    assert_eq!(page["partial"], true);
    assert_eq!(page["truncated"], true);
    assert_eq!(page["truncationReason"], "response_bytes");
    let (fixture, semantic) = incoming(requested);
    for (item, expected) in callers.iter().zip(&fixture.calls[..emitted]) {
        assert_eq!(item["callId"], expected.call.id);
    }
    let mut expected_proofs: BTreeSet<_> = fixture
        .selected
        .selected
        .provenance
        .iter()
        .map(|proof| proof.id.as_str())
        .collect();
    for (index, item) in fixture.calls[..emitted].iter().enumerate() {
        expected_proofs.insert(item.provenance.id.as_str());
        expected_proofs.extend(
            semantic
                .call(index)
                .unwrap()
                .proofs
                .iter()
                .map(|proof| proof.id.as_str()),
        );
    }
    let actual_proofs: BTreeSet<_> = page["data"]["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|proof| proof["id"].as_str().unwrap())
        .collect();
    assert_eq!(actual_proofs, expected_proofs);

    // Compact projection supplies the exact prefix items, document refs and
    // disclosure. Restore its omitted full evidence from this Rust-only fixture.
    let full_prefix = |count: usize| {
        let mut prefix = project(count, true);
        assert_eq!(prefix["data"]["callers"].as_array().unwrap().len(), count);
        let mut coverage = BTreeMap::new();
        let mut proofs = BTreeMap::new();
        for row in &fixture.selected.selected.coverage {
            coverage.insert(
                (row.producer_id.clone(), row.document_path.clone()),
                row.clone(),
            );
        }
        for proof in &fixture.selected.selected.provenance {
            proofs.insert(proof.id.clone(), proof.clone());
        }
        for (index, caller) in prefix["data"]["callers"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let item = &fixture.calls[index];
            let bound = semantic.call(index).unwrap();
            caller["binding"] = json!(bound.binding);
            caller["semanticBasis"] = json!(bound.provenance.basis);
            coverage.insert(
                (
                    item.coverage.producer_id.clone(),
                    item.coverage.document_path.clone(),
                ),
                item.coverage.clone(),
            );
            let tuple: crate::native_evidence::Coverage = wire_row(&bound.coverage).unwrap();
            coverage.insert(
                (tuple.producer_id.clone(), tuple.document_path.clone()),
                tuple,
            );
            proofs.insert(item.provenance.id.clone(), item.provenance.clone());
            for proof in &bound.proofs {
                proofs.insert(proof.id.clone(), wire_row(proof).unwrap());
            }
        }
        let (bases, mut proofs) = intern_bases(
            prefix["data"]["callers"].as_array_mut().unwrap(),
            proofs.into_values(),
        )
        .unwrap();
        label_projected_proofs(&mut proofs, &semantic, count).unwrap();
        prefix["data"]["semanticBases"] = json!(bases);
        prefix["data"]["coverage"] = json!(coverage.into_values().collect::<Vec<_>>());
        prefix["data"]["provenance"] = json!(proofs);
        prefix["truncationReason"] = json!("response_bytes");
        prefix
    };
    assert_eq!(full_prefix(emitted), page);
    assert!(
        frame_bytes(&full_prefix(emitted + 1)) > super::super::wire::RESPONSE_BYTES,
        "the next complete caller prefix must exceed the wire cap"
    );

    // Compare the same emitted prefix, not all requested callers, with repeated bases.
    let mut inlined = page.clone();
    let basis = basis();
    for caller in inlined["data"]["callers"].as_array_mut().unwrap() {
        let fields = caller.as_object_mut().unwrap();
        fields.remove("semanticBasisRef");
        fields.insert("semanticBasis".into(), basis.clone());
    }
    for proof in inlined["data"]["provenance"].as_array_mut().unwrap() {
        let fields = proof.as_object_mut().unwrap();
        let semantic = fields.remove("basisRef").unwrap() != Value::Null;
        fields.insert(
            "basis".into(),
            if semantic { basis.clone() } else { Value::Null },
        );
    }
    inlined["data"]
        .as_object_mut()
        .unwrap()
        .remove("semanticBases");
    let (now, before) = (frame_bytes(&page), frame_bytes(&inlined));
    assert!(
        now <= super::super::wire::RESPONSE_BYTES,
        "interned caller page is {now} bytes"
    );
    assert!(
        before > super::super::wire::RESPONSE_BYTES,
        "inlined caller page unexpectedly fits at {before} bytes"
    );
}

#[test]
fn projected_call_and_shared_proofs_keep_original_history() {
    let (result, mut semantic) = incoming(1);
    let projection = crate::model::SemanticProjection {
        current_pin: result.selected.selected.pin,
        semantic_basis_pin: crate::model::IndexPin {
            index_revision: result
                .selected
                .selected
                .pin
                .index_revision
                .saturating_sub(1),
            ..result.selected.selected.pin
        },
        basis_native_revision_id: "revision:v2:historical".into(),
        semantic_status: crate::model::SemanticStatus::OlderBasis,
    };
    let bound = semantic.calls[0].as_mut().unwrap();
    bound.projection = Some(projection.clone());
    bound.effective_freshness = crate::semantic_evidence::Freshness::PossiblyStale;
    let evidence = call_evidence(Some(bound), None).unwrap();
    assert_eq!(evidence.freshness, "possiblyStale");
    assert_eq!(evidence.historical_freshness, "fresh");
    let mut proofs: Vec<Value> = bound.proofs.iter().map(|proof| json!(proof)).collect();
    label_projected_proofs(&mut proofs, &semantic, 1).unwrap();
    for proof in proofs {
        assert_eq!(proof["projection"], json!(projection));
        assert_eq!(proof["freshness"], "fresh");
        assert_eq!(proof["historicalFreshness"], "fresh");
        assert_eq!(proof["effectiveFreshness"], "possiblyStale");
    }
}

#[test]
fn compact_fits_twenty_semantic_callers_under_25_kib() {
    let page = project(20, true);
    assert_eq!(page["data"]["callers"].as_array().unwrap().len(), 20);
    let bytes = frame_bytes(&page);
    assert!(bytes < 25 * 1024, "compact page is {bytes} bytes");
    let data = page["data"].as_object().unwrap();
    for field in ["coverage", "provenance", "semanticBases"] {
        assert!(!data.contains_key(field), "{field}");
    }
    for caller in page["data"]["callers"].as_array().unwrap() {
        assert!(caller.get("binding").is_none());
        assert!(caller.get("semanticBasisRef").is_none());
        assert_eq!(caller["disposition"], "resolved");
        assert_eq!(caller["evidenceTier"], "semantic");
        assert!(caller["targetId"].is_string());
    }
    // Completeness and warnings are those of the full answer.
    let full = project(20, false);
    assert_eq!(page["warnings"], full["warnings"]);
    assert_eq!(page["partial"], full["partial"]);
    // Twenty full callers overflow the cap; compact returns them all.
    assert_eq!(full["truncationReason"], "response_bytes");
    assert_eq!(page["truncationReason"], "limit");
    assert_eq!(page["data"]["scopeCoverage"], full["data"]["scopeCoverage"]);
}

/// #665: a rooted graph in the live shape of `DaemonState::enqueue_index`. The root
/// (`src/daemon/state.rs`) calls `fanout` helpers, each in its own file, and every
/// helper calls two leaves beside it. Every call has a resolved direct rust-analyzer
/// binding with its own proof, and every declaration its own declaration-binding proof,
/// all carrying the same imported basis.
fn graph(fanout: usize, max_nodes: u64, max_calls: u64) -> crate::graph_query::GraphAnswer {
    use crate::graph_query::{
        DocumentFacts, GraphFacts, GraphRequest, GraphSnapshot, SemanticDocument, evaluate,
    };
    use std::{collections::BTreeMap, sync::Arc};
    const PRODUCER: &str = "rust-analyzer";
    let mut facts = GraphFacts::complete(GraphSnapshot {
        source_set_id: SOURCE_SET.into(),
        revision_id: REVISION.into(),
    });
    facts
        .producers
        .insert(PRODUCER.into(), ["rust".to_owned()].into());
    let mut files: BTreeMap<String, (usize, Vec<(String, usize)>)> = BTreeMap::new();
    let mut edges: Vec<(String, String, usize)> = Vec::new();
    let root = "src/daemon/state.rs".to_owned();
    files
        .entry(root.clone())
        .or_insert((0, vec![]))
        .1
        .push(("enqueue_index".into(), 0));
    let mut next = 1;
    for i in 0..fanout {
        let path = format!("src/daemon/worker_{i}.rs");
        let file = i + 1;
        let helper = (format!("schedule_checkout_work_{i}"), next);
        next += 1;
        let entry = files.entry(path.clone()).or_insert((file, vec![]));
        entry.1.push(helper.clone());
        edges.push((root.clone(), path.clone(), helper.1));
        for j in 0..2 {
            let leaf = (format!("record_pending_request_{i}_{j}"), next);
            next += 1;
            files.get_mut(&path).unwrap().1.push(leaf.clone());
            edges.push((path.clone(), path.clone(), leaf.1));
        }
    }
    let sid = |n: usize| format!("sid:v1:{:032x}", 0x5000 + n);
    // Helpers are 1, 4, 7, ... (owned by the root, 0); leaves follow their helper.
    let owner_of = |n: usize| -> usize {
        if n == 0 || (n - 1).is_multiple_of(3) {
            0
        } else {
            n - (n - 1) % 3
        }
    };
    let mut documents: BTreeMap<String, DocumentFacts> = BTreeMap::new();
    for (path, (file, declarations)) in &files {
        let key: crate::native_evidence::DocumentKey =
            serde_json::from_value(document(path)).unwrap();
        let mut decls = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let mut declaration_bindings = Vec::new();
        for (name, n) in declarations {
            let start = 2000 * (n + 1);
            let declaration: crate::native_evidence::Declaration =
                serde_json::from_value(json!({"syntaxId":sid(*n),
                "document":document(path),"revisionId":REVISION,"kind":"method",
                "role":"production","name":name,"lookupKey":name,
                "ancestors":[{"kind":"implementation","name":"DaemonState",
                    "signature":null,"ordinal":0}],
                "key":{"kind":"method","name":name,"signature":null,"ordinal":0},
                "range":{"start":start,"end":start + 1800},
                "nameRange":{"start":start + 7,"end":start + 7 + name.len()},
                "header":{"kind":"method","name":name,"modifiers":["pub"],
                    "typeParameters":[],"parameters":[
                        {"name":"self","type":"&self","variadic":false},
                        {"name":"request","type":"IndexRequest","variadic":false}],
                    "resultType":"anyhow::Result<RequestId>","bases":[]},
                "provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
                .unwrap();
            decls.insert(sid(*n), declaration);
            let proof = hex(n + 40, 64);
            provenance.insert(
                proof.clone(),
                serde_json::from_value(json!({"id":proof,
                "producerId":PRODUCER,"document":document(path),"revisionId":REVISION,
                "contentHash":hex(file + 7, 64),"evidenceKind":"declarationBinding",
                "basis":basis(),"freshness":"fresh",
                "derivedFrom":{"factDigest":hex(n + 41, 64),"provenanceId":hex(n + 42, 64),
                    "recordKind":"occurrence"}}))
                .unwrap(),
            );
            declaration_bindings.push(serde_json::from_value(json!({"syntaxId":sid(*n),
                "symbols":[{"scheme":"scip","symbol":format!("rust-analyzer cargo trellis 0.1.0 daemon/{name}()."),
                    "scope":"global","document":null}],
                "join":{"anchor":{"contentHash":hex(file + 7, 64),"document":document(path),
                    "kind":"declarationName","range":{"start":start + 7,"end":start + 7 + name.len()},
                    "revisionId":REVISION},"candidateIds":[sid(*n)],"diagnostic":null,
                    "status":"exact"},"provenanceId":proof})).unwrap());
        }
        let mut calls: BTreeMap<String, Vec<crate::native_evidence::Call>> = BTreeMap::new();
        let mut call_bindings = Vec::new();
        for (ordinal_source, (from, to_path, target)) in edges.iter().enumerate() {
            if from != path {
                continue;
            }
            let owner = owner_of(*target);
            let owned = calls.entry(sid(owner)).or_default();
            let start = 2000 * (owner + 1) + 100 + 40 * owned.len();
            let call_id = format!("occ:v2:{:032x}", 0x9000 + ordinal_source);
            let target_name = &files[to_path].1.iter().find(|d| d.1 == *target).unwrap().0;
            owned.push(
                serde_json::from_value(json!({"id":call_id,"ownerSyntaxId":sid(owner),
                "ordinal":owned.len(),"document":document(path),"revisionId":REVISION,
                "range":{"start":start,"end":start + 30},
                "calleeRange":{"start":start,"end":start + target_name.len()},
                "spelling":target_name,"regionIds":[],
                "provenanceId":format!("native-proof:v1:{}", hex(file + 7, 64))}))
                .unwrap(),
            );
            let proof = hex(ordinal_source + 70, 64);
            provenance.insert(
                proof.clone(),
                serde_json::from_value(json!({"id":proof,
                "producerId":PRODUCER,"document":document(path),"revisionId":REVISION,
                "contentHash":hex(file + 7, 64),"evidenceKind":"semanticReference",
                "basis":basis(),"freshness":"fresh",
                "derivedFrom":{"factDigest":hex(ordinal_source + 71, 64),
                    "provenanceId":hex(ordinal_source + 72, 64),"recordKind":"occurrence"}}))
                .unwrap(),
            );
            call_bindings.push(
                serde_json::from_value(json!({"callId":call_id,"candidates":[],
                "declaredTarget":{"document":document(to_path),"kind":"internal",
                    "revisionId":REVISION,"syntaxId":sid(*target)},
                "dispatch":"direct",
                "join":{"anchor":{"contentHash":hex(file + 7, 64),"document":document(path),
                    "kind":"callee","range":{"start":start,"end":start + target_name.len()},
                    "revisionId":REVISION},"candidateIds":[call_id],"diagnostic":null,
                    "status":"exact"},
                "possibleDispatch":[],"possibleDispatchComplete":false,"provenanceId":proof,
                "resolution":"resolved","staleTarget":false}))
                .unwrap(),
            );
        }
        let semantic_coverage = serde_json::from_value(json!({"producerId":PRODUCER,
            "language":"rust","sourceSetId":SOURCE_SET,"documentPath":path,
            "revisionId":REVISION,"requested":true,"selected":true,"state":"complete",
            "supportedRoles":["definition","read","write","call","type","import","alias"],
            "observedRoles":["definition","read","call","type"],"diagnostic":null,
            "reasons":null}))
        .unwrap();
        for declaration in decls.keys() {
            facts
                .locations
                .insert(declaration.clone(), Some(key.clone()));
        }
        documents.insert(
            path.clone(),
            DocumentFacts {
                projection: None,
                key: key.clone(),
                declarations: decls,
                calls,
                native_coverage: serde_json::from_value(
                    serde_json::to_value(native_coverage(path)).unwrap(),
                )
                .unwrap(),
                native_provenance: serde_json::from_value(
                    serde_json::to_value(native_proof(path, *file)).unwrap(),
                )
                .unwrap(),
                semantic: Some(SemanticDocument {
                    projection: None,
                    coverage: semantic_coverage,
                    provenance,
                    call_bindings,
                    declaration_bindings,
                    symbols: vec![],
                    type_relationships: vec![],
                }),
            },
        );
    }
    for document in documents.into_values() {
        facts
            .documents
            .insert(document.key.clone(), Arc::new(document));
    }
    let requested = GraphRequest {
        source_set_id: SOURCE_SET.into(),
        revision_id: REVISION.into(),
        root_syntax_id: sid(0),
        semantic_producer_id: Some(PRODUCER.into()),
        depth: 2,
        max_nodes,
        max_calls,
        include_external: false,
        include_tests: false,
        app_paths: None,
    };
    let outcome = evaluate(&requested, &mut facts).unwrap();
    crate::graph_query::GraphAnswer {
        basis: serde_json::from_value(json!({
            "indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":6}))
        .unwrap(),
        requested,
        outcome,
        facts,
    }
}

fn graph_frame_bytes(envelope: &Value) -> usize {
    let frame = json!({"jsonrpc":"2.0","id":1,
        "result":crate::mcp::tools::result("trellis_call_graph", envelope.clone(), true)});
    serde_json::to_vec(&frame).unwrap().len() + 1
}

fn project_graph(answer: &mut crate::graph_query::GraphAnswer, compact: bool) -> Value {
    project_graph_keys(answer, compact, &graph_node_keys(answer))
}

/// Every node's stable lookup key, as the server reads them for a compact graph.
fn graph_node_keys(
    answer: &crate::graph_query::GraphAnswer,
) -> std::collections::BTreeMap<String, String> {
    let Ok(result) = &answer.outcome else {
        return Default::default();
    };
    result
        .nodes
        .iter()
        .map(|node| {
            let d = &node.declaration;
            let name = d.name.clone().unwrap_or_default();
            (d.syntax_id.clone(), format!("{}::{name}", d.document.path))
        })
        .collect()
}

fn project_graph_keys(
    answer: &mut crate::graph_query::GraphAnswer,
    compact: bool,
    node_keys: &std::collections::BTreeMap<String, String>,
) -> Value {
    super::call_graph(
        &json!(1),
        answer,
        Some("src/daemon/state.rs::DaemonState::enqueue_index"),
        node_keys,
        &Default::default(),
        compact,
        std::path::Path::new("/Users/someone/projects/trellis"),
        false,
        true,
    )
    .unwrap()
    .unwrap()
}

/// The pre-#665 wire shape of the same answer: every proof repeats its whole basis.
fn inline_graph_bases(envelope: &Value) -> Value {
    let mut inlined = envelope.clone();
    let bases = envelope["data"]["semanticBases"]
        .as_array()
        .unwrap()
        .clone();
    for proof in inlined["data"]["result"]["provenance"]
        .as_array_mut()
        .unwrap()
    {
        let fields = proof.as_object_mut().unwrap();
        let basis = match fields.remove("basisRef").unwrap() {
            Value::Null => Value::Null,
            reference => {
                let mut basis = bases[reference.as_u64().unwrap() as usize].clone();
                basis.as_object_mut().unwrap().remove("ref");
                basis
            }
        };
        fields.insert("basis".into(), basis);
    }
    let data = inlined["data"].as_object_mut().unwrap();
    data.remove("semanticBases");
    data.remove("responseOmitted");
    inlined
}

#[test]
fn historical_graph_proofs_share_bases_and_compact_keeps_projection_status() {
    use crate::semantic_evidence::{Freshness, ProjectedEvidence};
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut answer = graph(1, 150, 500);
    let projection = crate::model::SemanticProjection {
        current_pin: answer.basis,
        semantic_basis_pin: crate::model::IndexPin {
            index_revision: answer.basis.index_revision - 1,
            ..answer.basis
        },
        basis_native_revision_id: REVISION.into(),
        semantic_status: crate::model::SemanticStatus::OlderBasis,
    };
    let result = answer.outcome.as_mut().unwrap();
    let index = result
        .provenance
        .iter()
        .position(|proof| proof.basis.is_some())
        .unwrap();
    let proof = result.provenance.remove(index);
    let original = json!(proof);
    let status = crate::model::DocumentSemanticStatus {
        producer_id: proof.producer_id.clone(),
        language: proof.document.language.clone(),
        path: proof.document.path.clone(),
        projection: projection.clone(),
    };
    result.projected_provenance.push(ProjectedEvidence::new(
        proof,
        projection.clone(),
        Freshness::Fresh,
    ));
    result.semantic_projections.push(projection.clone());
    result.semantic_documents.push(status);
    result.edges[0].projection = Some(projection.clone());
    result.edges[0].historical_freshness = Some(Freshness::Fresh);
    result.edges[0].effective_freshness = Some(Freshness::PossiblyStale);

    let full = project_graph(&mut answer, false);
    let compact = project_graph(&mut answer, true);
    let bases = full["data"]["semanticBases"].as_array().unwrap();
    assert_eq!(bases.len(), 1);
    let projected = &full["data"]["result"]["projectedProvenance"][0];
    assert_eq!(projected["projection"], json!(projection));
    assert_eq!(projected["historicalFreshness"], "fresh");
    assert_eq!(projected["effectiveFreshness"], "possiblyStale");
    let mut restored = projected["evidence"].clone();
    let reference = restored
        .as_object_mut()
        .unwrap()
        .remove("basisRef")
        .unwrap();
    let mut basis = bases[reference.as_u64().unwrap() as usize].clone();
    basis.as_object_mut().unwrap().remove("ref");
    restored["basis"] = basis;
    assert_eq!(restored, original);
    assert!(
        full["data"]["result"]["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .any(|proof| proof["basisRef"] == reference)
    );
    let compact_result = &compact["data"]["result"];
    for field in ["provenance", "projectedProvenance", "coverage"] {
        assert!(compact_result.get(field).is_none());
    }
    assert!(compact["data"].get("semanticBases").is_none());
    for field in ["semanticProjections", "semanticDocuments"] {
        assert_eq!(compact_result[field], full["data"]["result"][field]);
    }
    for field in ["projection", "historicalFreshness", "effectiveFreshness"] {
        assert_eq!(
            compact_result["edges"][0][field],
            full["data"]["result"]["edges"][0][field]
        );
    }

    let script = r#"
const {createRequire} = require('node:module');
const {readFileSync} = require('node:fs');
const local = createRequire(process.cwd() + '/tests/mcp/package.json');
const Ajv = local('ajv/dist/2020.js').default;
const catalog = JSON.parse(readFileSync('tests/fixtures/mcp/catalog.json','utf8'));
const schema = catalog.find(tool => tool.name === 'trellis_call_graph').outputSchema;
const validate = new Ajv({strict:false}).compile(schema);
for (const output of JSON.parse(readFileSync(0,'utf8'))) {
  if (!validate(output)) throw Error(JSON.stringify(validate.errors));
}
"#;
    let mut child = Command::new("node")
        .args(["-e", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node is required by tools/verify");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&json!([full, compact])).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn graph_basis_table_emits_the_imported_basis_once_and_loses_nothing() {
    let mut answer = graph(3, 150, 500);
    let full = serde_json::to_value(answer.outcome.clone().unwrap()).unwrap();
    let page = project_graph(&mut answer, false);
    let result = &page["data"]["result"];
    assert_eq!(result["nodes"].as_array().unwrap().len(), 10);
    assert_eq!(result["edges"].as_array().unwrap().len(), 9);
    assert_eq!(page["truncationReason"], Value::Null);
    let bases = page["data"]["semanticBases"].as_array().unwrap();
    assert_eq!(bases.len(), 1);
    assert_eq!(bases[0]["ref"], 0);
    assert_eq!(bases[0]["producerHash"], hex(4, 64));
    let wire = serde_json::to_string(&page).unwrap();
    assert_eq!(wire.matches(&hex(4, 64)).count(), 1);
    let proofs = result["provenance"].as_array().unwrap();
    assert_eq!(proofs.len(), 23);
    for proof in proofs {
        assert!(proof.get("basis").is_none(), "{proof}");
        let expected = if proof["evidenceKind"] == "measuredSyntax" {
            json!(null)
        } else {
            json!(0)
        };
        assert_eq!(proof["basisRef"], expected);
    }
    // Re-inlining the table restores the evaluated result exactly.
    let old = inline_graph_bases(&page);
    assert_eq!(old["data"]["result"], full);
    // The live data point: ten nodes took ~60 KB, one cut from the cap.
    let (now, before) = (graph_frame_bytes(&page), graph_frame_bytes(&old));
    assert!(before > 60 * 1024, "the old shape is {before} bytes");
    assert!(
        now * 10 < before * 8,
        "{now} bytes is not much below {before}"
    );
}

#[test]
fn compact_graph_keeps_the_graph_and_drops_four_fifths_of_the_bytes() {
    let mut answer = graph(3, 150, 500);
    let full = project_graph(&mut answer, false);
    let page = project_graph(&mut answer, true);
    let old = graph_frame_bytes(&inline_graph_bases(&full));
    let bytes = graph_frame_bytes(&page);
    assert!(bytes * 5 < old, "compact {bytes} bytes, old full {old}");
    assert!(
        bytes * 4 < graph_frame_bytes(&full),
        "compact {bytes} bytes"
    );
    let data = page["data"].as_object().unwrap();
    assert!(!data.contains_key("semanticBases"));
    assert_eq!(data["responseOmitted"], json!({"nodes":0,"edges":0}));
    let result = page["data"]["result"].as_object().unwrap();
    assert!(!result.contains_key("coverage"));
    assert!(!result.contains_key("provenance"));
    let (graph, proofs) = (&page["data"]["result"], &full["data"]["result"]);
    let keys = graph_node_keys(&answer);
    for (node, whole) in graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .zip(proofs["nodes"].as_array().unwrap())
    {
        let declaration = node["declaration"].as_object().unwrap();
        let mut kept: Vec<_> = declaration.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            [
                "document",
                "kind",
                "lookupKey",
                "name",
                "range",
                "role",
                "stableLookupKey",
                "syntaxId"
            ]
        );
        // #715: every compact node keeps its durable follow-up handle.
        let sid = declaration["syntaxId"].as_str().unwrap();
        assert_eq!(declaration["stableLookupKey"], json!(keys[sid]));
        for field in kept.into_iter().filter(|field| *field != "stableLookupKey") {
            assert_eq!(declaration[field], whole["declaration"][field]);
        }
        assert!(whole["declaration"].get("stableLookupKey").is_none());
        assert_eq!(node["depth"], whole["depth"]);
    }
    // A node without a stable key (a partial Rust impl parse) says so with null.
    let unkeyed = project_graph_keys(&mut answer, true, &Default::default());
    for node in unkeyed["data"]["result"]["nodes"].as_array().unwrap() {
        assert_eq!(node["declaration"]["stableLookupKey"], Value::Null);
    }
    assert_catalog_valid("trellis_call_graph", &[&full, &page, &unkeyed]);
    let edges = graph["edges"].as_array().unwrap();
    assert_eq!(edges.len(), proofs["edges"].as_array().unwrap().len());
    for (edge, whole) in edges.iter().zip(proofs["edges"].as_array().unwrap()) {
        assert!(edge.get("binding").is_none());
        assert_eq!(edge["resolution"], whole["binding"]["resolution"]);
        assert_eq!(edge["resolution"], "resolved");
        for field in ["from", "to", "visit", "boundaryReason", "callSiteCount"] {
            assert_eq!(edge[field], whole[field], "{field}");
        }
        let call = edge["call"].as_object().unwrap();
        let mut kept: Vec<_> = call.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(
            kept,
            [
                "calleeRange",
                "id",
                "ordinal",
                "ownerSyntaxId",
                "range",
                "spelling"
            ]
        );
    }
    for field in [
        "frontier",
        "omitted",
        "hints",
        "warnings",
        "partial",
        "truncated",
    ] {
        assert_eq!(graph[field], proofs[field], "{field}");
    }
    for field in ["warnings", "partial", "truncated", "truncationReason"] {
        assert_eq!(page[field], full[field], "{field}");
    }
}

#[test]
fn default_graph_limits_fit_the_response_without_byte_clipping() {
    for compact in [false, true] {
        let mut args = json!({"schemaVersion":1,"symbolId":format!("sid:v1:{:032x}", 0x5000)});
        if compact {
            args["compact"] = json!(true);
        }
        let query = crate::mcp::tools::call_graph_query(&args);
        let limits = (query.max_nodes, query.max_calls);
        assert_eq!(limits, if compact { (50, 60) } else { (8, 10) });
        // A root with forty helpers that each call two more is cut by the limits only.
        let mut answer = graph(40, limits.0, limits.1);
        let page = project_graph(&mut answer, compact);
        let result = &page["data"]["result"];
        assert_eq!(result["nodes"].as_array().unwrap().len() as u64, limits.0);
        assert_eq!(result["edges"].as_array().unwrap().len() as u64, limits.1);
        assert_eq!(page["truncationReason"], "limit", "compact={compact}");
        assert_eq!(
            page["data"]["responseOmitted"],
            json!({"nodes":0,"edges":0})
        );
        let bytes = graph_frame_bytes(&page);
        // #715: compact node keys add about 3 KB at fifty nodes; the cap is 64 KiB.
        assert!(bytes < 60 * 1024, "compact={compact}: {bytes} bytes");
    }
}

#[test]
fn byte_clipping_reports_the_omitted_nodes_and_edges() {
    for compact in [false, true] {
        let mut answer = graph(40, 150, 500);
        let full = answer.outcome.clone().unwrap();
        assert_eq!((full.nodes.len(), full.edges.len()), (121, 120));
        let page = project_graph(&mut answer, compact);
        let result = &page["data"]["result"];
        let nodes = result["nodes"].as_array().unwrap().len();
        let edges = result["edges"].as_array().unwrap().len();
        assert!(nodes < 121 && edges < 120, "{nodes} {edges}");
        assert_eq!(page["truncationReason"], "response_bytes");
        assert_eq!(
            page["data"]["responseOmitted"],
            json!({"nodes":121 - nodes,"edges":120 - edges})
        );
        assert!(result["omitted"]["responseBytes"].as_u64().unwrap() > 0);
        assert!(graph_frame_bytes(&page) <= crate::mcp::wire::RESPONSE_BYTES);
        let hint = if compact {
            ""
        } else {
            "; compact:true fits more"
        };
        let summary = crate::mcp::tools::result("trellis_call_graph", page, true);
        assert_eq!(
            summary["content"][0]["text"],
            format!(
                "trellis_call_graph: {edges} edges, truncated: response_bytes ({} nodes, {} \
                     edges omitted{hint}); full result in structuredContent.",
                121 - nodes,
                120 - edges
            )
        );
    }
}

/// Validate tool outputs against the reviewed catalog fixture's closed output schema.
pub(super) fn assert_catalog_valid(tool: &str, outputs: &[&Value]) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let script = r#"
const {createRequire} = require('node:module');
const {readFileSync} = require('node:fs');
const local = createRequire(process.cwd() + '/tests/mcp/package.json');
const Ajv = local('ajv/dist/2020.js').default;
const [tool, outputs] = JSON.parse(readFileSync(0,'utf8'));
const catalog = JSON.parse(readFileSync('tests/fixtures/mcp/catalog.json','utf8'));
const validate = new Ajv({strict:false}).compile(catalog.find(t => t.name === tool).outputSchema);
for (const output of outputs) {
  if (!validate(output)) throw Error(JSON.stringify(validate.errors));
}
"#;
    let mut child = Command::new("node")
        .args(["-e", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Node is required by tools/verify");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&json!([tool, outputs])).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// #724: only a fresh, direct binding to another located declaration excludes a caller.
/// A binding to the selected symbol, a dispatch boundary, a possibly stale or stale
/// binding and a binding without a located target keep the caller listed.
#[test]
fn only_a_fresh_direct_binding_elsewhere_excludes_a_caller() {
    use crate::graph_query::BoundaryReason;
    use crate::semantic_evidence::Freshness;
    let (result, semantic) = incoming(1);
    let selected = result.selected.selected.declaration.syntax_id.clone();
    let elsewhere = Some("sid:v1:2301cdb0e7c351db88fecd8aa523edc9".to_owned());
    let base = semantic.calls[0].clone().unwrap();
    let case = |target: Option<String>, boundary, freshness| {
        let mut bound = base.clone();
        bound.target_id = target;
        bound.boundary_reason = boundary;
        bound.effective_freshness = freshness;
        bound.bound_elsewhere(&selected)
    };
    assert!(case(
        elsewhere.clone(),
        BoundaryReason::None,
        Freshness::Fresh
    ));
    assert!(!case(
        Some(selected.clone()),
        BoundaryReason::None,
        Freshness::Fresh
    ));
    assert!(!case(
        elsewhere.clone(),
        BoundaryReason::Dispatch,
        Freshness::Fresh
    ));
    assert!(!case(
        elsewhere.clone(),
        BoundaryReason::None,
        Freshness::PossiblyStale
    ));
    assert!(!case(
        elsewhere.clone(),
        BoundaryReason::Stale,
        Freshness::Stale
    ));
    assert!(!case(None, BoundaryReason::None, Freshness::Fresh));
}

/// #724: the store's count of callers excluded as bound elsewhere is emitted as
/// `data.boundElsewhereOmitted`, in full and compact form, and validates against the
/// closed schema. Without exclusions the field is absent and the page is unchanged.
#[test]
fn incoming_reports_callers_omitted_as_bound_elsewhere() {
    let run = |omitted: u64, compact: bool| {
        let (mut result, semantic) = incoming(4);
        result.bound_elsewhere_omitted = omitted;
        super::incoming(
            &json!(1),
            result,
            &semantic,
            &super::test_support::rust_semantic("fresh", [326, 14, 312, 0, 29]),
            compact,
            std::path::Path::new("/Users/someone/projects/trellis"),
            false,
            true,
        )
        .unwrap()
        .unwrap()
    };
    for compact in [false, true] {
        let none = run(0, compact);
        assert!(none["data"].get("boundElsewhereOmitted").is_none());
        let page = run(2, compact);
        assert_eq!(page["data"]["boundElsewhereOmitted"], 2);
        let mut expected = none.clone();
        expected["data"]["boundElsewhereOmitted"] = json!(2);
        assert_eq!(page, expected, "only the count is added");
        assert_catalog_valid("trellis_incoming_calls", &[&none, &page]);
    }
}

/// #724: a scalar that crosses byte 1024 is dropped whole, not split: 1023 ASCII bytes
/// then a two-byte `é` (bytes 1023..1025) clip to the 1023 ASCII bytes.
#[test]
fn call_graph_spelling_clip_backs_off_a_scalar_crossing_the_cap() {
    let mut answer = graph(1, 150, 500);
    let spelling = format!("{}é", "x".repeat(1023));
    assert_eq!(spelling.len(), 1025);
    answer.outcome.as_mut().unwrap().edges[0].call.spelling = Some(spelling);
    for compact in [false, true] {
        let page = project_graph(&mut answer, compact);
        let call = &page["data"]["result"]["edges"][0]["call"];
        assert_eq!(call["spelling"], json!("x".repeat(1023)));
        assert_eq!(call["spellingTruncated"], true);
    }
}

/// #724: a graph edge's call spelling is clipped to `calleeTextBytes` (1024) on a
/// scalar boundary and flagged `spellingTruncated:true`, in full and compact form. An
/// edge that fits, even exactly, is byte-identical and carries no flag.
#[test]
fn call_graph_edge_spelling_is_clipped_to_the_callee_text_cap() {
    let spell = |answer: &mut crate::graph_query::GraphAnswer, edge: usize, text: String| {
        answer.outcome.as_mut().unwrap().edges[edge].call.spelling = Some(text);
    };
    for compact in [false, true] {
        let mut baseline = graph(2, 150, 500);
        spell(&mut baseline, 1, "x".repeat(1024));
        let baseline = project_graph(&mut baseline, compact);
        let mut answer = graph(2, 150, 500);
        // 768 two-byte scalars: 1536 bytes, the dogfood closure's size.
        spell(&mut answer, 0, "é".repeat(768));
        spell(&mut answer, 1, "x".repeat(1024));
        let page = project_graph(&mut answer, compact);
        let edges = page["data"]["result"]["edges"].as_array().unwrap();
        assert_eq!(edges[0]["call"]["spelling"], json!("é".repeat(512)));
        assert_eq!(edges[0]["call"]["spellingTruncated"], true);
        assert_eq!(edges[1]["call"]["spelling"], json!("x".repeat(1024)));
        for edge in &edges[1..] {
            assert!(edge["call"].get("spellingTruncated").is_none());
        }
        let mut expected = baseline.clone();
        let call = &mut expected["data"]["result"]["edges"][0]["call"];
        call["spelling"] = json!("é".repeat(512));
        call["spellingTruncated"] = json!(true);
        assert_eq!(page, expected, "only the clipped edge changes");
        assert_catalog_valid("trellis_call_graph", &[&page]);
        // Before #724 the whole spelling was emitted: clipping 512 bytes adds the flag.
        let mut before = baseline.clone();
        before["data"]["result"]["edges"][0]["call"]["spelling"] = json!("é".repeat(768));
        let (before, after) = (graph_frame_bytes(&before), graph_frame_bytes(&page));
        eprintln!("#724 graph compact={compact}: before {before} B, after {after} B");
        assert_eq!(after + 512, before + r#","spellingTruncated":true"#.len());
    }
}

fn coverage_document(
    language: &'static str,
    path: String,
    failed: bool,
) -> crate::store::CoverageDocument {
    crate::store::CoverageDocument {
        language,
        path,
        failed,
        reasons: vec![if failed {
            "notInProducerOutput"
        } else {
            "nativeGaps"
        }],
    }
}

fn ready_describe(id: &Value) -> Value {
    let basis = json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":4});
    describe(
        id,
        "project",
        "ready",
        None,
        Some(([[0; 5]; 5], Tiers::default())),
        Some(basis),
        false,
    )
}

/// #780: describe's coverage document pages concatenate to the whole sorted list exactly
/// once, honour `limit` (default 20) and the page target, and an offset past the end is
/// `range_too_large` with the admitted basis.
#[test]
fn coverage_document_pages_concatenate_within_limit_and_page_target() {
    let id = json!(7);
    let documents: Vec<_> = (0..45)
        .map(|n| coverage_document("rust", format!("src/f{n:02}.rs"), n < 5))
        .collect();
    let read = |request: Value| {
        coverage_documents(&id, ready_describe(&id), documents.clone(), &request, true)
    };
    let first = read(json!({}));
    let page = &first["data"]["coverageDocuments"];
    assert_eq!(page["page"], json!({"offset":0,"nextOffset":20,"total":45}));
    assert_eq!(page["items"].as_array().unwrap().len(), 20);
    assert_eq!(
        page["items"][0],
        json!({"language":"rust","path":"src/f00.rs","state":"failed","reasons":["notInProducerOutput"]})
    );
    assert_eq!(page["items"][5]["state"], "partial");
    let mut offset = 0;
    let mut seen = Vec::new();
    loop {
        let answer = read(json!({"limit":7,"offset":offset}));
        let page = &answer["data"]["coverageDocuments"];
        assert!(page["items"].as_array().unwrap().len() <= 7);
        assert_eq!(page["page"]["total"], 45);
        seen.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["path"].clone()),
        );
        match page["page"]["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    let whole: Vec<_> = documents
        .iter()
        .map(|document| json!(document.path))
        .collect();
    assert_eq!(seen, whole);
    // The end itself is an empty last page; one past it is typed.
    let end = read(json!({"offset":45}));
    assert_eq!(end["data"]["coverageDocuments"]["items"], json!([]));
    assert_eq!(
        end["data"]["coverageDocuments"]["page"]["nextOffset"],
        Value::Null
    );
    let past = read(json!({"offset":46}));
    assert_eq!(past["error"]["code"], "range_too_large");
    assert_eq!(
        past["error"]["currentBasis"],
        ready_describe(&id)["evidenceBasis"]
    );
    // Long paths stop a page at the page target, never before its first document.
    let long: Vec<_> = (0..50)
        .map(|n| coverage_document("python", format!("{n:02}/{}", "p".repeat(3000)), false))
        .collect();
    let answer = coverage_documents(&id, ready_describe(&id), long, &json!({"limit":50}), true);
    let shown = answer["data"]["coverageDocuments"]["items"]
        .as_array()
        .unwrap()
        .len();
    assert!((1..50).contains(&shown), "{shown}");
    let bytes = serde_json::to_vec(&super::super::tools::result(
        "trellis_workspace_describe",
        answer.clone(),
        true,
    ))
    .unwrap()
    .len();
    assert!(bytes <= super::super::architecture::PAGE_BYTES, "{bytes}");
    assert_eq!(
        answer["data"]["coverageDocuments"]["page"]["nextOffset"],
        shown
    );
}

/// #780 review (PR #832): the page is sized against the final response line, with the
/// real `searchIndex` configuration, `workspace` and `catchingUp` attached. Prime2's
/// legal scenario (`prime2-review/coverage-page-sizing.py`): a 41-byte root, a 500-byte
/// models directory, a 564-byte model path and fifty failed 400-byte paths whose
/// components are at most 99 bytes. Sizing before that metadata admitted 44 documents
/// (a 24,972-byte line); the final line with 43 is 24,505 bytes.
#[test]
fn coverage_document_page_fits_the_final_answer_with_search_configuration() {
    let id = json!(7);
    let workspace = "/Users/developer/projects/coverage-review";
    assert_eq!(workspace.len(), 41);
    let zero = json!({"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0});
    let mut languages: Vec<Value> = LANGUAGES
        .iter()
        .map(|language| {
            json!({"language":language,"nativeCoverageTier":"syntax","coverage":zero,
                "semantic":{"available":false,"producerId":null,"freshness":"unavailable",
                    "coverage":zero,
                    "note":"semantic producer does not cover this language; showing syntax",
                    "partialReasons":null,"reimport":null,"documentStatus":{"olderBasis":0,"catchingUp":0,"needsReimport":0}}})
        })
        .collect();
    languages[1]["coverage"] =
        json!({"selected":50,"complete":50,"partial":0,"failed":0,"unselected":0});
    languages[1]["semantic"]["producerId"] = json!("fixture");
    languages[1]["semantic"]["note"] = Value::Null;
    languages[1]["semantic"]["coverage"] =
        json!({"selected":50,"complete":0,"partial":0,"failed":50,"unselected":0});
    let models = format!(
        "/tmp/{}models{}",
        format!("{}/", "m".repeat(99)).repeat(4),
        "x".repeat(89)
    );
    let model_path =
        format!("{models}/voyageai/voyage-4-nano/5fb34e870f49f19e79c94d5e118f5ee730133279");
    assert_eq!((models.len(), model_path.len()), (500, 564));
    let mut ready = describe(&id, "coverage-review", "ready", None, None, None, false);
    ready["evidenceBasis"] =
        json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":4});
    ready["data"]["languages"] = json!(languages);
    ready["partial"] = json!(true);
    ready["data"]["searchIndex"] = json!({"backend":"cpu",
        "unavailableReason":"model_not_configured",
        "configuration":{"configFile":"/Users/developer/.config/trellis/config.toml",
            "modelsDir":models,"modelName":"voyageai/voyage-4-nano",
            "modelRevision":"5fb34e870f49f19e79c94d5e118f5ee730133279",
            "modelPath":model_path,"backend":"auto",
            "sources":{"modelsDir":"config_file","model":"default","backend":"default"}},
        "state":"unavailable","declarations":50,"embedded":0});
    ready["workspace"] = json!(workspace);
    ready["catchingUp"] = json!(false);
    let documents: Vec<_> = (0..50)
        .map(|n| {
            let path = format!(
                "src/{n:02}/{}{}.rs",
                format!("{}/", "d".repeat(99)).repeat(3),
                "f".repeat(90)
            );
            assert_eq!(path.len(), 400);
            crate::store::CoverageDocument {
                language: "rust",
                path,
                failed: true,
                reasons: vec!["other"],
            }
        })
        .collect();
    let request = json!({"limit":50});
    let target = super::super::architecture::PAGE_BYTES;
    let answer = coverage_documents(&id, ready.clone(), documents.clone(), &request, true);
    let shown = answer["data"]["coverageDocuments"]["items"]
        .as_array()
        .unwrap()
        .len();
    let bytes = line_bytes(&id, &answer, true);
    assert_eq!((shown, bytes), (42, 24_353));
    assert!(bytes <= target);
    assert_eq!(
        answer["data"]["coverageDocuments"]["page"]["nextOffset"],
        42
    );
    // One more document would not fit: the page is the largest that does.
    let mut over = answer.clone();
    over["data"]["coverageDocuments"]["items"]
        .as_array_mut()
        .unwrap()
        .push(json!({"language":"rust","path":documents[42].path,"state":"failed","reasons":["other"]}));
    over["data"]["coverageDocuments"]["page"]["nextOffset"] = json!(43);
    assert_eq!(line_bytes(&id, &over, true), 24_820);
    // Negative control: sizing before the search configuration and workspace are
    // attached, as 647751cd did, admits more documents and a final line over target.
    let mut bare = ready.clone();
    bare["data"]["searchIndex"] = json!({"backend":null,"unavailableReason":null});
    for field in ["workspace", "catchingUp"] {
        bare.as_object_mut().unwrap().remove(field);
    }
    let early = coverage_documents(&id, bare, documents, &request, true);
    let mut early_final = early.clone();
    early_final["data"]["searchIndex"] = ready["data"]["searchIndex"].clone();
    early_final["workspace"] = json!(workspace);
    early_final["catchingUp"] = json!(false);
    let early_shown = early["data"]["coverageDocuments"]["items"]
        .as_array()
        .unwrap()
        .len();
    assert!(early_shown > shown, "{early_shown}");
    assert!(line_bytes(&id, &early_final, true) > target);
}
