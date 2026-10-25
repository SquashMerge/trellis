use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    capture::Capture,
    indexer::IndexOptions,
    native_evidence as native, native_ids,
    scip_admission::{
        CapturedScipInventory, ReferenceProfileAdmission, TrustedSelection,
        decode_reference_profile,
    },
    semantic_evidence as semantic,
    semantic_import::{MeasuredFacts, join_inventory},
};

const WIRE: &str = "122e0a087372632f412e6a73220a6a617661736372697074120b0a0300090a1202412318011a090a0241232811320166";

fn fixture() -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    let text = "function f(){g();}\n";
    fs::write(physical.join("src/A.js"), text).unwrap();
    fs::write(physical.join("artifact.scip"), hex::decode(WIRE).unwrap()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(physical.clone());
    options.scip_path = Some(physical.join("artifact.scip"));
    options.manifest_path = Some(physical.join("manifest.json"));
    let first = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let first_native =
        native::from_capture(&first, &physical, &identity.record_id, &cancel).unwrap();
    let (set, rev) = (first_native.source_set, first_native.revision);
    let file = &first.files[0];
    let doc = json!({"language":"javascript","relativePath":"src/A.js","contentHash":file.hash});
    let source_manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(
            &json!([{"document":{"sourceSetId":set.id,"language":"javascript","path":"src/A.js"},"contentHash":file.hash}]),
        ),
    );
    let profile: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let producer = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    let bytes = hex::decode(WIRE).unwrap();
    let uri = format!("file://{}", physical.display());
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
        "producer":{"name":producer["name"],"version":producer["version"],"executableHash":producer["executableHash"],"positionEncoding":2},
        "sourceSet":{"id":set.id,"rootId":set.root_id,"languages":set.languages,"dependencies":set.dependencies},
        "sourceRevision":rev.id,"sourceManifestHash":source_manifest_hash,
        "components":{"toolchainHash":rev.toolchain_hash,"configHash":rev.config_hash,"dependencyHash":rev.dependency_hash},
        "metadata":{"protocolVersion":0,"projectRoot":uri,"textDocumentEncoding":1},
        "sourceDocuments":[doc],"documents":[doc],"exclusions":[],
        "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"", &bytes),"language":"javascript","documents":["src/A.js"]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(physical.join("manifest.json"), &manifest_bytes).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let selection = TrustedSelection {
        expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
        profile_id: "mac-mini-m5-pro-v1".into(),
        producer_name: "scip-typescript".into(),
    };
    let admission =
        ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel).unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let sources = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    (root, native, sources, inventory)
}

#[test]
fn authenticated_javascript_claims_join_real_native_candidates() {
    let (_root, native, sources, inventory) = fixture();
    assert_eq!(inventory.claims.len(), 2);
    let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        facts
            .declaration_bindings
            .iter()
            .any(|b| b.join.status == semantic::JoinStatus::Exact)
    );
    assert!(
        facts
            .joins
            .iter()
            .all(|(_, join)| join.candidate_ids.len() <= 1)
    );
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    let r = native
        .declarations
        .iter()
        .find_map(|d| d.name_range.clone())
        .unwrap();
    assert_eq!(
        measured
            .join(&doc, r, semantic::JoinKind::DeclarationName)
            .unwrap()
            .status,
        semantic::JoinStatus::Exact
    );
}

#[test]
fn unavailable_capability_is_not_inferred_from_missing_candidate() {
    let (_root, native, sources, _inventory) = fixture();
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    let join = measured
        .join(
            &doc,
            native::Range { start: 0, end: 8 },
            semantic::JoinKind::Callee,
        )
        .unwrap();
    assert_eq!(join.status, semantic::JoinStatus::Unmatched);
    assert_eq!(join.candidate_ids, Vec::<String>::new());
}

#[test]
fn admitted_raw_coordinates_reject_split_utf16_scalar_and_crlf() {
    use trellis::semantic_import::occurrence_range;
    let (_root, _native, _sources, inventory) = fixture();
    let mut claim = inventory
        .claims
        .iter()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap()
        .clone();
    let text = "😀\r\nBase".as_bytes();
    claim.envelope.raw["range"] = json!({"encoding":"multiLine","startLine":1,"startCharacter":0,"endLine":1,"endCharacter":4});
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 6, end: 10 }
    );
    claim.envelope.raw["range"] =
        json!({"encoding":"singleLine","line":0,"startCharacter":0,"endCharacter":1});
    assert!(
        occurrence_range(&claim, text, 2)
            .unwrap_err()
            .to_string()
            .contains("splits scalar")
    );
    claim.envelope.raw["range"] =
        json!({"encoding":"singleLine","line":0,"startCharacter":0,"endCharacter":2});
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 0, end: 4 }
    );
    assert!(occurrence_range(&claim, text, 1).is_err());
    claim.envelope.position_encoding = 0;
    assert_eq!(
        occurrence_range(&claim, text, 2).unwrap(),
        native::Range { start: 0, end: 4 }
    );
}

#[test]
fn four_measured_families_retain_exact_native_identity_and_capability_authority() {
    let (_root, native, sources, _inventory) = fixture();
    let doc = native.revision.documents[0].key.clone();
    let measured = MeasuredFacts::new(&native, &sources).unwrap();
    for d in native
        .declarations
        .iter()
        .filter(|d| d.document == doc && d.name_range.is_some())
    {
        let join = measured
            .join(
                &doc,
                d.name_range.clone().unwrap(),
                semantic::JoinKind::DeclarationName,
            )
            .unwrap();
        assert_eq!(join.status, semantic::JoinStatus::Exact);
        assert_eq!(join.candidate_ids, vec![d.syntax_id.clone()]);
    }
    for call in native.calls.iter().filter(|c| c.document == doc) {
        let invocation = measured
            .join(&doc, call.range.clone(), semantic::JoinKind::Invocation)
            .unwrap();
        assert_eq!(invocation.candidate_ids, vec![call.id.clone()]);
        if let Some(callee) = &call.callee_range {
            let join = measured
                .join(&doc, callee.clone(), semantic::JoinKind::Callee)
                .unwrap();
            assert_eq!(join.candidate_ids, vec![call.id.clone()]);
        }
    }
    for reference in native.references.iter().filter(|r| r.document == doc) {
        let join = measured
            .join(&doc, reference.range.clone(), semantic::JoinKind::Reference)
            .unwrap();
        assert_eq!(join.candidate_ids, vec![reference.id.clone()]);
    }
    let absent = native::Range { start: 0, end: 8 };
    assert_eq!(
        measured
            .join(&doc, absent.clone(), semantic::JoinKind::Callee)
            .unwrap()
            .status,
        semantic::JoinStatus::Unmatched
    );
    let mut gap = native.clone();
    let support = gap.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Callee)
        .unwrap();
    support.gaps.push(native::MeasurementGap {
        range: absent.clone(),
        diagnostic: "opaque_native_construct".into(),
    });
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(&doc, absent.clone(), semantic::JoinKind::Callee)
            .unwrap()
            .status,
        semantic::JoinStatus::Unsupported
    );
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(
                &doc,
                native::Range { start: 0, end: 9 },
                semantic::JoinKind::Callee
            )
            .unwrap()
            .status,
        semantic::JoinStatus::Unmatched
    );
    let support = gap.capability_reports[0]
        .families
        .iter_mut()
        .find(|f| f.kind == native::MeasurementKind::Callee)
        .unwrap();
    support.gaps.clear();
    support.available = false;
    support.diagnostic = Some("no_native_convention".into());
    assert_eq!(
        MeasuredFacts::new(&gap, &sources)
            .unwrap()
            .join(&doc, absent, semantic::JoinKind::Callee)
            .unwrap()
            .diagnostic
            .as_deref(),
        Some("no_native_convention")
    );
}

#[test]
fn duplicate_and_conflicting_authenticated_claims_preserve_raw_proofs() {
    let (_root, native, sources, inventory) = fixture();
    let original = inventory
        .claims
        .iter()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap()
        .clone();
    let mut same = inventory.clone();
    same.claims.push(original.clone());
    assert!(
        join_inventory(&same, &native, &sources, 2).is_err(),
        "duplicate raw proof must fail closed"
    );
    let mut competing = inventory.clone();
    let mut other = original.clone();
    other.envelope.raw["symbol"] = json!("external pkg Other#");
    other.canonical_bytes = native_ids::canonical(
        &json!({"formatVersion":1,"recordKind":"occurrence","relativePath":other.envelope.relative_path,
        "positionEncoding":other.envelope.position_encoding,"raw":other.envelope.raw}),
    );
    other.fact_digest = native_ids::digest(b"trellis.semantic-fact.v1\0", &other.canonical_bytes);
    other.captured_proof_id = native_ids::digest(
        b"trellis.capture-proof.v1\0",
        &native_ids::canonical(&json!({
        "producerId":"scip-typescript","artifactHash":inventory.artifact_hash,"sourceSetId":native.source_set.id,
        "sourceRevision":native.revision.id,"language":other.language,"relativePath":other.envelope.relative_path,
        "factDigest":other.fact_digest})),
    );
    let other_proof = other.captured_proof_id.clone();
    competing.claims.push(other);
    let output = join_inventory(&competing, &native, &sources, 2).unwrap();
    let ids: Vec<_> = output
        .joins
        .iter()
        .filter(|(_, j)| j.anchor.kind == semantic::JoinKind::DeclarationName)
        .map(|(proof, j)| (proof.as_str(), j.status))
        .collect();
    assert!(ids.iter().any(|(id, _)| *id == original.captured_proof_id));
    assert!(ids.iter().any(|(id, _)| *id == other_proof));
}

#[test]
fn reference_and_call_roles_are_not_authorized_by_native_shape_alone() {
    let (_root, native, sources, inventory) = fixture();
    let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        facts.call_bindings.is_empty(),
        "no authenticated call occurrence in pinned fixture"
    );
    assert!(
        facts
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
    for (proof, join) in &facts.joins {
        let claim = inventory
            .claims
            .iter()
            .find(|c| &c.captured_proof_id == proof)
            .unwrap();
        assert_eq!(claim.envelope.record_kind, "occurrence");
        assert_eq!(join.anchor.document.language, "javascript");
    }
}

type ScipSymbolFixture<'a> = (&'a str, i32, Vec<(&'a str, bool, bool)>);

fn admitted_case(
    language: &str,
    source: &str,
    occurrences: Vec<(usize, usize, &str, i32)>,
    symbols: Vec<ScipSymbolFixture<'_>>,
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    admitted_case_with_calls(language, source, occurrences, symbols, &[])
}
fn admitted_case_with_calls(
    language: &str,
    source: &str,
    occurrences: Vec<(usize, usize, &str, i32)>,
    symbols: Vec<ScipSymbolFixture<'_>>,
    semantic_calls: &[usize],
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    use protobuf::{EnumOrUnknown, Message};
    use scip::types::{self};
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    fs::create_dir(physical.join("src")).unwrap();
    let (path, producer_name, encoding) = match language {
        "java" => ("src/A.java", "scip-java", 2),
        "javascript" => ("src/A.js", "scip-typescript", 2),
        "python" => ("src/A.py", "scip-python", 2),
        "rust" => ("src/A.rs", "rust-analyzer", 1),
        _ => panic!("unsupported test language"),
    };
    fs::write(physical.join(path), source).unwrap();
    let mut index = types::Index::new();
    let mut document = types::Document::new();
    document.language = language.into();
    document.relative_path = path.into();
    document.position_encoding = EnumOrUnknown::from_i32(encoding);
    for (start, end, symbol, roles) in occurrences {
        let mut occurrence = types::Occurrence::new();
        let prefix = &source[..start];
        let line = prefix.bytes().filter(|c| *c == b'\n').count();
        let col = prefix.rsplit('\n').next().unwrap().encode_utf16().count();
        let width = source[start..end].encode_utf16().count();
        let col = if encoding == 1 {
            prefix.rsplit('\n').next().unwrap().len()
        } else {
            col
        };
        let width = if encoding == 1 { end - start } else { width };
        occurrence.range = vec![line as i32, col as i32, (col + width) as i32];
        occurrence.symbol = symbol.into();
        occurrence.symbol_roles = roles;
        if semantic_calls.contains(&start) {
            let close = source[end..].find(')').unwrap() + end + 1;
            let enclosing_width = if encoding == 1 {
                close - start
            } else {
                source[start..close].encode_utf16().count()
            };
            occurrence.enclosing_range =
                vec![line as i32, col as i32, (col + enclosing_width) as i32];
        }
        occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
        document.occurrences.push(occurrence);
    }
    for (symbol, kind, relationships) in symbols {
        let mut info = types::SymbolInformation::new();
        info.symbol = symbol.into();
        info.kind = EnumOrUnknown::from_i32(kind);
        info.display_name = symbol.into();
        for (other, reference, implementation) in relationships {
            let mut edge = types::Relationship::new();
            edge.symbol = other.into();
            edge.is_reference = reference;
            edge.is_implementation = implementation;
            info.relationships.push(edge);
        }
        document.symbols.push(info);
    }
    index.documents.push(document);
    let bytes = index.write_to_bytes().unwrap();
    fs::write(physical.join("artifact.scip"), &bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(physical.clone());
    options.scip_path = Some(physical.join("artifact.scip"));
    options.manifest_path = Some(physical.join("manifest.json"));
    let first = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let first_native =
        native::from_capture(&first, &physical, &identity.record_id, &cancel).unwrap();
    let set = &first_native.source_set;
    let rev = &first_native.revision;
    let f = &first.files[0];
    let doc = json!({"language":language,"relativePath":path,"contentHash":f.hash});
    let manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(
            &json!([{"document":{"sourceSetId":set.id,"language":language,"path":path},"contentHash":f.hash}]),
        ),
    );
    let profile: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let producer = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == producer_name)
        .unwrap();
    let manifest = json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
        "producer":{"name":producer["name"],"version":producer["version"],"executableHash":producer["executableHash"],"positionEncoding":encoding},
        "sourceSet":{"id":set.id,"rootId":set.root_id,"languages":set.languages,"dependencies":set.dependencies},
        "sourceRevision":rev.id,"sourceManifestHash":manifest_hash,
        "components":{"toolchainHash":rev.toolchain_hash,"configHash":rev.config_hash,"dependencyHash":rev.dependency_hash},
        "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",physical.display()),"textDocumentEncoding":1},
        "sourceDocuments":[doc],"documents":[doc],"exclusions":[],
        "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"",&bytes),"language":language,"documents":[path]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    fs::write(physical.join("manifest.json"), &manifest_bytes).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let selection = TrustedSelection {
        expected_manifest_hash: native_ids::digest(b"", &manifest_bytes),
        profile_id: "mac-mini-m5-pro-v1".into(),
        producer_name: producer_name.into(),
    };
    let admission =
        ReferenceProfileAdmission::from_trusted_selection(&capture, &selection, &cancel).unwrap();
    let inventory = decode_reference_profile(&admission, &cancel).unwrap();
    let native = native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
    let sources = capture
        .files
        .iter()
        .map(|f| {
            (
                native::DocumentKey {
                    source_set_id: native.revision.source_set_id.clone(),
                    language: f.language.clone(),
                    path: f.path.clone(),
                },
                f.text.as_bytes().to_vec(),
            )
        })
        .collect();
    (root, native, sources, inventory)
}

#[test]
fn four_real_admitted_languages_measure_all_anchor_families() {
    for (language, source, name) in [
        ("java", "class A { void f() { f(); } }", "f"),
        ("javascript", "function f(){ f(); }", "f"),
        ("python", "def f():\n    f()\n", "f"),
        ("rust", "fn f(){ f(); }", "f"),
    ] {
        let occurrence = source.rfind(name).unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(occurrence, occurrence + 1, "external pkg f.", 0)],
            vec![],
        );
        let key = native.revision.documents[0].key.clone();
        let measured = MeasuredFacts::new(&native, &sources).unwrap();
        assert!(
            native.declarations.iter().any(|d| d.name_range.is_some()),
            "{language}"
        );
        for declaration in native
            .declarations
            .iter()
            .filter(|d| d.name_range.is_some())
        {
            let join = measured
                .join(
                    &key,
                    declaration.name_range.clone().unwrap(),
                    semantic::JoinKind::DeclarationName,
                )
                .unwrap();
            assert!(
                join.candidate_ids.contains(&declaration.syntax_id),
                "{language}"
            );
        }
        assert!(!native.calls.is_empty(), "{language}");
        for call in &native.calls {
            assert!(
                measured
                    .join(&key, call.range.clone(), semantic::JoinKind::Invocation)
                    .unwrap()
                    .candidate_ids
                    .contains(&call.id),
                "{language}"
            );
            if let Some(range) = &call.callee_range {
                assert!(
                    measured
                        .join(&key, range.clone(), semantic::JoinKind::Callee)
                        .unwrap()
                        .candidate_ids
                        .contains(&call.id),
                    "{language}"
                );
            }
        }
        for reference in &native.references {
            assert!(
                measured
                    .join(&key, reference.range.clone(), semantic::JoinKind::Reference)
                    .unwrap()
                    .candidate_ids
                    .contains(&reference.id),
                "{language}"
            );
        }
        let output = join_inventory(
            &inventory,
            &native,
            &sources,
            if language == "rust" { 1 } else { 2 },
        )
        .unwrap();
        assert!(
            output
                .joins
                .iter()
                .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Reference
                    && j.status == semantic::JoinStatus::Exact),
            "{language}"
        );
        let reference = output
            .references
            .iter()
            .find(|r| r.range.start == occurrence)
            .expect("actual semantic reference");
        let actual = native
            .references
            .iter()
            .find(|r| r.id == reference.id)
            .unwrap();
        assert_eq!(
            (
                &reference.owner_syntax_id,
                reference.ordinal,
                &reference.spelling,
                &reference.range
            ),
            (
                &actual.owner_syntax_id,
                actual.ordinal,
                &actual.spelling,
                &actual.range
            ),
            "{language} native identity"
        );
        assert_eq!(
            reference.resolution,
            semantic::Resolution::External,
            "{language} explicit global target"
        );
        assert!(
            !reference.roles.contains(&semantic::Role::Call),
            "{language} no raw call assertion"
        );
    }
}

#[test]
fn two_distinct_native_ids_are_ambiguous_but_repeated_raw_proof_is_not() {
    let source = "function f(){ f(); }";
    let offset = source.rfind('f').unwrap();
    let (_root, mut native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![(offset, offset + 1, "external pkg f.", 0)],
        vec![],
    );
    let doc = native.revision.documents[0].key.clone();
    let r = native
        .references
        .iter()
        .find(|r| r.range.start == offset)
        .unwrap()
        .clone();
    let actual = MeasuredFacts::new(&native, &sources)
        .unwrap()
        .join(&doc, r.range.clone(), semantic::JoinKind::Reference)
        .unwrap();
    assert_eq!(actual.status, semantic::JoinStatus::Exact);
    let mut other = r.clone();
    other.id = native.calls[0].id.clone();
    native.references.push(other);
    let joined = MeasuredFacts::new(&native, &sources)
        .unwrap()
        .join(&doc, r.range.clone(), semantic::JoinKind::Reference)
        .unwrap();
    assert_eq!(joined.status, semantic::JoinStatus::Ambiguous);
    assert_eq!(joined.candidate_ids.len(), 2);
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output.references.is_empty(),
        "ambiguous source candidate cannot enrich reference"
    );
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Reference
                && j.status == semantic::JoinStatus::Ambiguous)
    );
}

#[test]
fn distinct_admitted_proofs_on_one_anchor_preserve_joins_and_ambiguity() {
    let source = "function useIt(){ value; }";
    let offset = source.find("value").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (offset, offset + 5, "external pkg A#", 0),
            (offset, offset + 5, "external pkg B#", 0),
        ],
        vec![],
    );
    assert_eq!(
        inventory
            .claims
            .iter()
            .filter(|c| c.envelope.record_kind == "occurrence")
            .count(),
        2
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let proofs: std::collections::BTreeSet<_> = output
        .joins
        .iter()
        .filter(|(_, j)| {
            j.anchor.kind == semantic::JoinKind::Reference
                && j.status == semantic::JoinStatus::Exact
        })
        .map(|(p, _)| p.as_str())
        .collect();
    assert_eq!(
        proofs.len(),
        2,
        "each distinct authenticated raw proof retains its own join"
    );
    assert_eq!(
        output.references.len(),
        1,
        "one native reference identity selects one valid fact"
    );
    assert_eq!(
        output.references[0].resolution,
        semantic::Resolution::Ambiguous
    );
    assert_eq!(output.references[0].candidates.len(), 2);
    assert!(proofs.contains(output.references[0].provenance_id.as_str()));
}

#[test]
fn admitted_semantic_call_witness_controls_binding_and_role() {
    let source = "function f(){ f(); }";
    let offset = source.rfind('f').unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![(offset, offset + 1, "external pkg f.", 0)],
        vec![],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output.call_bindings.is_empty(),
        "plain reference is not semantic call"
    );
    assert!(
        output
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
    let (_root, native, sources, inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![(offset, offset + 1, "external pkg f.", 8)],
        vec![],
        &[offset],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(
        output.call_bindings.len(),
        1,
        "authenticated semantic call should bind exact native callee"
    );
    assert!(output.references[0].roles.contains(&semantic::Role::Call));
}

#[test]
fn admitted_java_direct_class_and_external_type_are_source_backed() {
    let source = "class Base {} class Child extends Base {} class Cat implements java.lang.Comparable<Cat> {}";
    let at = |text: &str| {
        let i = source.find(text).unwrap();
        (i, i + text.len())
    };
    let child = at("Child");
    let base_decl = at("Base");
    let base_use = {
        let i = source.rfind("Base").unwrap();
        (i, i + 4)
    };
    let cat = at("Cat");
    let comparable = at("Comparable");
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        vec![
            (base_decl.0, base_decl.1, "scip-java pkg Base#", 1),
            (child.0, child.1, "scip-java pkg Child#", 1),
            (base_use.0, base_use.1, "scip-java pkg Base#", 8),
            (cat.0, cat.1, "scip-java pkg Cat#", 1),
            (
                comparable.0,
                comparable.1,
                "scip-java java/lang/ Comparable#",
                8,
            ),
        ],
        vec![
            ("scip-java pkg Base#", 7, vec![]),
            (
                "scip-java pkg Child#",
                7,
                vec![("scip-java pkg Base#", true, false)],
            ),
            (
                "scip-java pkg Cat#",
                7,
                vec![("scip-java java/lang/ Comparable#", true, true)],
            ),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .type_relationships
            .iter()
            .any(|r| r.kind == semantic::RelationshipKind::Extends
                && matches!(r.target, semantic::Target::Internal { .. })),
        "local direct extends: {:#?}",
        output.type_relationships
    );
    assert!(
        output
            .type_relationships
            .iter()
            .any(|r| r.kind == semantic::RelationshipKind::Implements
                && matches!(r.target, semantic::Target::External { .. })),
        "external direct implements: {:#?}",
        output.type_relationships
    );
}

#[test]
fn admitted_java_direct_interface_enum_record_and_transitive_negative() {
    let source = "interface I {} interface J extends I {} enum E implements I { A } record R() implements I {} class Base {} class Mid extends Base {} class Leaf extends Mid {}";
    let mut occ = Vec::new();
    for (name, symbol) in [
        ("I", "scip-java pkg I#"),
        ("J", "scip-java pkg J#"),
        ("E", "scip-java pkg E#"),
        ("R", "scip-java pkg R#"),
        ("Base", "scip-java pkg Base#"),
        ("Mid", "scip-java pkg Mid#"),
        ("Leaf", "scip-java pkg Leaf#"),
    ] {
        let first = source
            .find(&format!("{name} "))
            .or_else(|| source.find(&format!("{name}()")))
            .unwrap();
        occ.push((first, first + name.len(), symbol, 1));
    }
    for (needle, symbol) in [
        ("extends I", "scip-java pkg I#"),
        ("implements I", "scip-java pkg I#"),
        ("extends Base", "scip-java pkg Base#"),
        ("extends Mid", "scip-java pkg Mid#"),
    ] {
        let mut start = 0;
        while let Some(pos) = source[start..].find(needle) {
            let at = start + pos + needle.len()
                - symbol
                    .rsplit(' ')
                    .next()
                    .unwrap()
                    .trim_end_matches('#')
                    .len();
            let len = needle.rsplit(' ').next().unwrap().len();
            occ.push((at, at + len, symbol, 8));
            start = start + pos + needle.len();
        }
    }
    let symbols = vec![
        ("scip-java pkg I#", 21, vec![]),
        (
            "scip-java pkg J#",
            21,
            vec![("scip-java pkg I#", true, false)],
        ),
        (
            "scip-java pkg E#",
            11,
            vec![
                ("scip-java pkg I#", true, true),
                ("scip-java java/lang/Enum#", true, false),
            ],
        ),
        (
            "scip-java pkg R#",
            7,
            vec![("scip-java pkg I#", true, true)],
        ),
        ("scip-java pkg Base#", 7, vec![]),
        (
            "scip-java pkg Mid#",
            7,
            vec![("scip-java pkg Base#", true, false)],
        ),
        (
            "scip-java pkg Leaf#",
            7,
            vec![
                ("scip-java pkg Mid#", true, false),
                ("scip-java pkg Base#", true, false),
            ],
        ),
    ];
    let (_root, native, sources, inventory) = admitted_case("java", source, occ, symbols);
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let names: Vec<_> = output
        .type_relationships
        .iter()
        .map(|r| (r.kind, r.provenance_id.as_str()))
        .collect();
    assert_eq!(
        names.len(),
        6,
        "direct J->I, E->I, E->Enum, R->I, Mid->Base and Leaf->Mid only: {names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|(kind, _)| *kind == semantic::RelationshipKind::Implements)
            .count(),
        2
    );
    assert_eq!(
        names
            .iter()
            .filter(|(kind, _)| *kind == semantic::RelationshipKind::Extends)
            .count(),
        4
    );
}

#[test]
fn admitted_java_local_forward_override_keeps_full_opaque_overload() {
    let source = "class Base { void f(int x) {} void f(String x) {} } class Child extends Base { @Override void f(int x) {} }";
    let at = |needle: &str| source.find(needle).unwrap();
    let base = at("Base");
    let child = at("Child");
    let base_use = source.rfind("Base").unwrap();
    let f1 = at("f(int");
    let f2 = at("f(String");
    let f3 = source.rfind("f(int").unwrap();
    let b = "scip-java pkg Base#";
    let c = "scip-java pkg Child#";
    let first = "scip-java pkg Base#f(I).";
    let other = "scip-java pkg Base#f(Ljava/lang/String;).";
    let derived = "scip-java pkg Child#f(I).";
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        vec![
            (base, base + 4, b, 1),
            (child, child + 5, c, 1),
            (base_use, base_use + 4, b, 8),
            (f1, f1 + 1, first, 1),
            (f2, f2 + 1, other, 1),
            (f3, f3 + 1, derived, 1),
        ],
        vec![
            (b, 7, vec![]),
            (c, 7, vec![(b, true, false)]),
            (first, 26, vec![(derived, true, true)]),
            (other, 26, vec![]),
            (derived, 26, vec![(first, true, true), (other, true, true)]),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let methods: Vec<_> = output
        .type_relationships
        .iter()
        .filter(|r| r.kind == semantic::RelationshipKind::Overrides)
        .collect();
    assert_eq!(
        methods.len(),
        1,
        "forward only, identical overload: {:?}; diagnostics {:?}",
        methods,
        output.diagnostics
    );
    assert!(matches!(
        methods[0].target,
        semantic::Target::Internal { .. }
    ));
    let direct_proof = inventory
        .claims
        .iter()
        .find(|claim| {
            claim.envelope.record_kind == "relationship"
                && claim.envelope.raw["sourceSymbol"].as_str() == Some(derived)
                && claim.envelope.raw["targetSymbol"].as_str() == Some(first)
        })
        .unwrap();
    let reverse = inventory
        .claims
        .iter()
        .find(|claim| {
            claim.envelope.record_kind == "relationship"
                && claim.envelope.raw["sourceSymbol"].as_str() == Some(first)
                && claim.envelope.raw["targetSymbol"].as_str() == Some(derived)
        })
        .unwrap();
    let mismatch = inventory
        .claims
        .iter()
        .find(|claim| {
            claim.envelope.record_kind == "relationship"
                && claim.envelope.raw["sourceSymbol"].as_str() == Some(derived)
                && claim.envelope.raw["targetSymbol"].as_str() == Some(other)
        })
        .unwrap();
    assert_eq!(methods[0].provenance_id, direct_proof.captured_proof_id);
    assert!(
        output
            .type_relationships
            .iter()
            .all(|edge| edge.provenance_id != reverse.captured_proof_id
                && edge.provenance_id != mismatch.captured_proof_id)
    );
    assert!(
        output.diagnostics.is_empty(),
        "reverse/local descriptor mismatch is provably skipped, not partial"
    );
}

#[test]
fn admitted_java_comparable_positive_and_implicit_object_partial() {
    let source = "class Cat implements Comparable<Cat> { public int compareTo(Cat other) { return 0; } public String toString(){ return null; } }";
    let at = |needle: &str| source.find(needle).unwrap();
    let cat = at("Cat");
    let comparable = at("Comparable");
    let compare = at("compareTo");
    let string = at("toString");
    let cat_sym = "scip-java pkg Cat#";
    let compare_sym = "scip-java pkg Cat#compareTo(Lpkg/Cat;).";
    let comparable_sym = "scip-java java/lang/Comparable#";
    let external_compare = "scip-java java/lang/Comparable#compareTo().";
    let object_string = "scip-java java/lang/Object#toString().";
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        vec![
            (cat, cat + 3, cat_sym, 1),
            (comparable, comparable + 10, comparable_sym, 8),
            (compare, compare + 9, compare_sym, 1),
            (string, string + 8, "scip-java pkg Cat#toString().", 1),
        ],
        vec![
            (cat_sym, 7, vec![(comparable_sym, true, true)]),
            (compare_sym, 26, vec![(external_compare, true, true)]),
            (
                "scip-java pkg Cat#toString().",
                26,
                vec![(object_string, true, true)],
            ),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let overrides: Vec<_> = output
        .type_relationships
        .iter()
        .filter(|r| r.kind == semantic::RelationshipKind::Overrides)
        .collect();
    assert_eq!(
        overrides.len(),
        1,
        "Comparable positive, Object not measurable: {:?}",
        overrides
    );
    assert!(matches!(
        overrides[0].target,
        semantic::Target::External { .. }
    ));
    assert!(
        output
            .diagnostics
            .values()
            .any(|d| d.contains("unmeasurable_override_endpoint")),
        "Object missing endpoint is partial"
    );
}

#[test]
fn admitted_script_direct_local_external_and_reverse_transitive_exclusion() {
    for (language, source, classes, occ, edges) in [
        (
            "javascript",
            "class Base {} class Mid extends Base {} class Leaf extends Mid {} class X extends Outside {}",
            vec![
                ("Base", "typescript pkg Base#"),
                ("Mid", "typescript pkg Mid#"),
                ("Leaf", "typescript pkg Leaf#"),
                ("X", "typescript pkg X#"),
            ],
            vec![
                ("Base", "typescript pkg Base#"),
                ("Mid", "typescript pkg Mid#"),
                ("Outside", "typescript ext Outside#"),
            ],
            vec![
                ("typescript pkg Mid#", "typescript pkg Base#"),
                ("typescript pkg Leaf#", "typescript pkg Mid#"),
                ("typescript pkg Leaf#", "typescript pkg Base#"),
                ("typescript pkg Base#", "typescript pkg Mid#"),
                ("typescript pkg X#", "typescript ext Outside#"),
            ],
        ),
        (
            "python",
            "class Base: pass\nclass Mid(Base): pass\nclass Leaf(Mid): pass\nclass X(Outside): pass\n",
            vec![
                ("Base", "python pkg Base#"),
                ("Mid", "python pkg Mid#"),
                ("Leaf", "python pkg Leaf#"),
                ("X", "python pkg X#"),
            ],
            vec![
                ("Base", "python pkg Base#"),
                ("Mid", "python pkg Mid#"),
                ("Outside", "python ext Outside#"),
            ],
            vec![
                ("python pkg Mid#", "python pkg Base#"),
                ("python pkg Leaf#", "python pkg Mid#"),
                ("python pkg Leaf#", "python pkg Base#"),
                ("python pkg Base#", "python pkg Mid#"),
                ("python pkg X#", "python ext Outside#"),
            ],
        ),
    ] {
        let mut occurrences = Vec::new();
        let mut symbols = Vec::new();
        for &(name, symbol) in &classes {
            let declaration = source.find(&format!("class {name}")).unwrap() + 6;
            occurrences.push((declaration, declaration + name.len(), symbol, 1));
            let relationships = edges
                .iter()
                .filter(|(a, _)| a == &symbol)
                .map(|(_, b)| (*b, true, false))
                .collect();
            symbols.push((symbol, 7, relationships));
        }
        for (name, symbol) in occ {
            let use_offset = source.rfind(name).unwrap();
            occurrences.push((use_offset, use_offset + name.len(), symbol, 8));
        }
        let (_root, native, sources, inventory) =
            admitted_case(language, source, occurrences, symbols);
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        let doc = native.revision.documents[0].key.clone();
        let declaration = |symbol: &str| {
            let proof = inventory
                .claims
                .iter()
                .find(|c| {
                    c.envelope.record_kind == "occurrence"
                        && c.envelope.raw["symbol"].as_str() == Some(symbol)
                        && c.envelope.raw["symbolRoles"] == 1
                })
                .unwrap();
            output
                .declaration_bindings
                .iter()
                .find(|b| b.provenance_id == proof.captured_proof_id)
                .unwrap()
                .syntax_id
                .clone()
                .unwrap()
        };
        let direct = [
            (classes[1].1, classes[0].1),
            (classes[2].1, classes[1].1),
            (classes[3].1, "external"),
        ];
        assert_eq!(
            output.type_relationships.len(),
            3,
            "{language} direct only: {:?}",
            output.type_relationships
        );
        for (source_symbol, target_symbol) in direct {
            let claim = inventory
                .claims
                .iter()
                .find(|c| {
                    c.envelope.record_kind == "relationship"
                        && c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
                        && (target_symbol == "external"
                            || c.envelope.raw["targetSymbol"].as_str() == Some(target_symbol))
                })
                .unwrap();
            let edge = output
                .type_relationships
                .iter()
                .find(|e| e.provenance_id == claim.captured_proof_id)
                .expect("exact direct proof");
            assert_eq!(edge.kind, semantic::RelationshipKind::Extends);
            assert_eq!(
                edge.source,
                semantic::Target::Internal {
                    syntax_id: declaration(source_symbol),
                    document: doc.clone(),
                    revision_id: native.revision.id.clone()
                }
            );
            if target_symbol == "external" {
                assert!(matches!(edge.target, semantic::Target::External { .. }));
            } else {
                assert_eq!(
                    edge.target,
                    semantic::Target::Internal {
                        syntax_id: declaration(target_symbol),
                        document: doc.clone(),
                        revision_id: native.revision.id.clone()
                    }
                );
            }
        }
        for (source_symbol, target_symbol) in
            [(classes[2].1, classes[0].1), (classes[0].1, classes[1].1)]
        {
            let claim = inventory
                .claims
                .iter()
                .find(|c| {
                    c.envelope.record_kind == "relationship"
                        && c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
                        && c.envelope.raw["targetSymbol"].as_str() == Some(target_symbol)
                })
                .unwrap();
            assert!(
                output
                    .type_relationships
                    .iter()
                    .all(|edge| edge.provenance_id != claim.captured_proof_id),
                "{language} reverse/transitive must not publish"
            );
        }
    }
}

#[test]
fn admitted_rust_impl_and_supertrait_without_raw_edges_never_fabricate() {
    let source = "trait Parent {} trait Child: Parent {} struct S; impl Child for S {}";
    let (_root, native, sources, inventory) = admitted_case("rust", source, vec![], vec![]);
    assert!(
        inventory
            .claims
            .iter()
            .all(|c| c.envelope.record_kind != "relationship")
    );
    let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
    assert!(output.type_relationships.is_empty());
}

#[test]
fn admitted_declaration_roles_and_local_unresolved_external_targets() {
    let source = "class Local {} function useIt(){ Local; missing; }";
    let declare = source.find("Local").unwrap();
    let use_local = source.rfind("Local").unwrap();
    let missing = source.find("missing").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (declare, declare + 5, "local 1", 1),
            (use_local, use_local + 5, "local 1", 8),
            (missing, missing + 7, "external pkg Missing#", 8),
        ],
        vec![("local 1", 7, vec![])],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let local = output
        .references
        .iter()
        .find(|r| r.range.start == use_local)
        .unwrap();
    assert_eq!(local.resolution, semantic::Resolution::Resolved);
    let external = output
        .references
        .iter()
        .find(|r| r.range.start == missing)
        .unwrap();
    assert_eq!(external.resolution, semantic::Resolution::External);
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == declare && r.roles.contains(&semantic::Role::Definition))
    );
}

#[test]
fn admitted_unresolved_local_reference_and_noncall_method_value() {
    let source = "function f(){ f; missing; }";
    let f_use = source.rfind('f').unwrap();
    let missing = source.find("missing").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (f_use, f_use + 1, "local absent", 8),
            (missing, missing + 7, "local absent2", 8),
        ],
        vec![],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .references
            .iter()
            .all(|r| r.resolution == semantic::Resolution::Unresolved)
    );
    assert!(output.call_bindings.is_empty());
    assert!(
        output
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
}

#[test]
fn invocation_only_exact_join_can_bind_without_exact_callee() {
    let source = "function f(){ f(); }";
    let at = source.rfind('f').unwrap();
    let (_root, mut native, sources, inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![(at, at + 1, "external pkg f.", 8)],
        vec![],
        &[at],
    );
    let call = native
        .calls
        .iter_mut()
        .find(|c| c.callee_range.as_ref().is_some_and(|r| r.start == at))
        .unwrap();
    call.callee_range = None;
    let call_id = call.id.clone();
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Invocation
                && j.status == semantic::JoinStatus::Exact)
    );
    assert!(
        output
            .call_bindings
            .iter()
            .any(
                |binding| binding.join.anchor.kind == semantic::JoinKind::Invocation
                    && binding.call_id.as_ref() == Some(&call_id)
            )
    );
}

#[test]
fn published_native_v9_capability_families_authorize_only_available_measurements() {
    for (language, source) in [
        ("java", "class A {}"),
        ("javascript", "class A {}"),
        ("python", "class A: pass\n"),
        ("rust", "struct A;"),
    ] {
        let (_root, native, sources, _inventory) = admitted_case(language, source, vec![], vec![]);
        let key = native.revision.documents[0].key.clone();
        let report = native
            .capability_reports
            .iter()
            .find(|r| r.document == key)
            .unwrap();
        for family in &report.families {
            let measured = MeasuredFacts::new(&native, &sources).unwrap();
            let probe = native::Range { start: 0, end: 1 };
            let kind = match family.kind {
                native::MeasurementKind::DeclarationName => semantic::JoinKind::DeclarationName,
                native::MeasurementKind::Callee => semantic::JoinKind::Callee,
                native::MeasurementKind::Invocation => semantic::JoinKind::Invocation,
                native::MeasurementKind::Reference => semantic::JoinKind::Reference,
            };
            let result = measured.join(&key, probe, kind).unwrap();
            assert_eq!(
                result.status,
                if family.available {
                    semantic::JoinStatus::Unmatched
                } else {
                    semantic::JoinStatus::Unsupported
                },
                "{language}: {:?}",
                family.kind
            );
        }
    }
}

#[test]
fn invocation_without_matching_reference_never_enriches_call() {
    let source = "function f(){ f(); }";
    let at = source.rfind('f').unwrap();
    let (_root, mut native, sources, inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![(at, at + 1, "external pkg f.", 8)],
        vec![],
        &[at],
    );
    native.references.retain(|r| r.range.start != at);
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(output.call_bindings.is_empty());
    assert!(output.references.is_empty());
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Invocation
                && j.status == semantic::JoinStatus::Exact)
    );
}

#[test]
fn admitted_distinct_call_claims_reconcile_ambiguous_targets() {
    let source = "function f(){ f(); }";
    let at = source.rfind('f').unwrap();
    let (_root, native, sources, inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![
            (at, at + 1, "external pkg A.", 8),
            (at, at + 1, "external pkg B.", 8),
        ],
        vec![],
        &[at],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(output.call_bindings.len(), 2);
    assert!(
        output
            .call_bindings
            .iter()
            .all(|b| b.resolution == semantic::Resolution::Ambiguous && b.candidates.len() == 2)
    );
    assert_eq!(output.references.len(), 1);
    assert_eq!(
        output.references[0].resolution,
        semantic::Resolution::Ambiguous
    );
}

#[test]
fn call_role_requires_the_same_exact_raw_invocation_as_binding() {
    let source = "function f(){ f(); }";
    let at = source.rfind('f').unwrap();
    let (_root, native, sources, mut inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![(at, at + 1, "external pkg f.", 8)],
        vec![],
        &[at],
    );
    let raw = inventory
        .claims
        .iter_mut()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap();
    raw.envelope.raw["enclosingRange"] = json!({"encoding":"legacy","values":[0,at,at+1]});
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(output.call_bindings.is_empty());
    assert!(
        output
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Invocation
                && j.status == semantic::JoinStatus::Unmatched)
    );
}

#[test]
fn admitted_type_use_roles_require_exact_direct_clause_and_raw_identity() {
    for (language, source, owner, base) in [
        (
            "java",
            "class Base {} class Child extends Base {}",
            "Child",
            "Base",
        ),
        (
            "javascript",
            "class Base {} class Child extends Base {}",
            "Child",
            "Base",
        ),
        (
            "python",
            "class Base: pass\nclass Child(Base): pass\n",
            "Child",
            "Base",
        ),
        (
            "rust",
            "trait Base {} trait Child: Base {}",
            "Child",
            "Base",
        ),
    ] {
        let owner_at = source.find(owner).unwrap();
        let base_decl = source.find(base).unwrap();
        let base_use = source.rfind(base).unwrap();
        let sym_base = "global pkg Base#";
        let sym_child = "global pkg Child#";
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![
                (owner_at, owner_at + owner.len(), sym_child, 1),
                (base_decl, base_decl + base.len(), sym_base, 1),
                (base_use, base_use + base.len(), sym_base, 8),
            ],
            vec![
                (sym_child, 7, vec![(sym_base, true, false)]),
                (sym_base, 7, vec![]),
            ],
        );
        let output = join_inventory(
            &inventory,
            &native,
            &sources,
            if language == "rust" { 1 } else { 2 },
        )
        .unwrap();
        let reference = output
            .references
            .iter()
            .find(|r| r.range.start == base_use)
            .expect("native exact type reference");
        assert!(
            reference.roles.contains(&semantic::Role::Type),
            "{language}: {:?}",
            reference.roles
        );
    }
}

#[test]
fn admitted_import_alias_requires_exact_native_alias_declaration() {
    for (language, source, alias) in [
        (
            "javascript",
            "import {original as local} from 'pkg'; local;",
            "local",
        ),
        (
            "rust",
            "use pkg::original as local; fn f(){ let _ = local; }",
            "local",
        ),
    ] {
        let at = source.find(alias).unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(at, at + alias.len(), "local alias", 1 | 2)],
            vec![("local alias", 0, vec![])],
        );
        assert!(
            native
                .declarations
                .iter()
                .any(|d| d.kind == "alias" && d.name_range.as_ref().is_some_and(|r| r.start == at)),
            "{language} exact alias declaration"
        );
        let output = join_inventory(
            &inventory,
            &native,
            &sources,
            if language == "rust" { 1 } else { 2 },
        )
        .unwrap();
        let reference = output
            .references
            .iter()
            .find(|r| r.range.start == at)
            .expect("alias reference");
        assert!(
            reference.roles.contains(&semantic::Role::Definition),
            "{language}: {:?}",
            reference.roles
        );
        assert!(
            reference.roles.contains(&semantic::Role::Import),
            "{language}: {:?}",
            reference.roles
        );
        assert!(
            reference.roles.contains(&semantic::Role::Alias),
            "{language}: {:?}",
            reference.roles
        );
    }
}

#[test]
fn non_alias_python_and_ordinary_import_never_invent_alias_role() {
    for (language, source, name) in [
        ("python", "import pkg as local\nlocal\n", "local"),
        (
            "java",
            "import pkg.Local; class A { Local value; }",
            "Local",
        ),
    ] {
        let at = source.find(name).unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(at, at + name.len(), "global pkg Local#", 2 | 1)],
            vec![],
        );
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        assert!(
            output
                .references
                .iter()
                .all(|r| !r.roles.contains(&semantic::Role::Alias)),
            "{language}"
        );
    }
}

#[test]
fn admitted_java_transitive_override_claim_skips_without_partial() {
    let source = "class Base { void f(int x) {} } class Mid extends Base { @Override void f(int x) {} } class Leaf extends Mid { @Override void f(int x) {} }";
    let b = "scip-java pkg Base#";
    let m = "scip-java pkg Mid#";
    let l = "scip-java pkg Leaf#";
    let fb = "scip-java pkg Base#f(I).";
    let fm = "scip-java pkg Mid#f(I).";
    let fl = "scip-java pkg Leaf#f(I).";
    let word = |term: &str| {
        source
            .match_indices(term)
            .map(|(at, _)| at)
            .collect::<Vec<_>>()
    };
    let mut occurrences = vec![];
    for (name, symbol) in [("Base", b), ("Mid", m), ("Leaf", l)] {
        let spans = word(name);
        occurrences.push((spans[0], spans[0] + name.len(), symbol, 1));
        if spans.len() > 1 {
            occurrences.push((spans[1], spans[1] + name.len(), symbol, 8));
        }
    }
    let fs = word("f(int");
    for (idx, symbol) in [fb, fm, fl].iter().enumerate() {
        occurrences.push((fs[idx], fs[idx] + 1, *symbol, 1));
    }
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        occurrences,
        vec![
            (b, 7, vec![]),
            (m, 7, vec![(b, true, false)]),
            (l, 7, vec![(m, true, false)]),
            (fb, 26, vec![]),
            (fm, 26, vec![(fb, true, true)]),
            (fl, 26, vec![(fm, true, true), (fb, true, true)]),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let transitive = inventory
        .claims
        .iter()
        .find(|c| {
            c.envelope.record_kind == "relationship"
                && c.envelope.raw["sourceSymbol"].as_str() == Some(fl)
                && c.envelope.raw["targetSymbol"].as_str() == Some(fb)
        })
        .unwrap();
    assert!(
        output
            .type_relationships
            .iter()
            .all(|e| e.provenance_id != transitive.captured_proof_id)
    );
    let method_edges: Vec<_> = output
        .type_relationships
        .iter()
        .filter(|e| e.kind == semantic::RelationshipKind::Overrides)
        .collect();
    assert_eq!(
        method_edges.len(),
        2,
        "only local direct overrides: {method_edges:?}"
    );
    for (source_symbol, target_symbol) in [(fm, fb), (fl, fm)] {
        let proof = inventory
            .claims
            .iter()
            .find(|c| {
                c.envelope.record_kind == "relationship"
                    && c.envelope.raw["sourceSymbol"].as_str() == Some(source_symbol)
                    && c.envelope.raw["targetSymbol"].as_str() == Some(target_symbol)
            })
            .unwrap();
        let edge = method_edges
            .iter()
            .find(|e| e.provenance_id == proof.captured_proof_id)
            .expect("exact forward proof");
        assert_eq!(edge.kind, semantic::RelationshipKind::Overrides);
        assert!(
            matches!(edge.source, semantic::Target::Internal { .. })
                && matches!(edge.target, semantic::Target::Internal { .. })
        );
    }
    assert!(
        output.diagnostics.is_empty(),
        "known transitive owner skips without partial"
    );
}

#[test]
fn callback_and_method_reference_never_become_calls_from_native_shape() {
    for (language, source, needle) in [
        (
            "javascript",
            "function f() {} function use(g){ g(f); }",
            "f);",
        ),
        (
            "java",
            "class A { void f(){} void use(){ Runnable r = this::f; } }",
            "f;",
        ),
    ] {
        let at = source.rfind(needle).unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(at, at + 1, "external pkg f.", 8)],
            vec![],
        );
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        assert!(
            output
                .references
                .iter()
                .all(|r| !r.roles.contains(&semantic::Role::Call)),
            "{language}"
        );
        assert!(output.call_bindings.is_empty(), "{language}");
    }
}

#[test]
fn import_bit_without_measured_alias_declaration_never_grants_alias() {
    for (language, source, needle) in [
        (
            "javascript",
            "import Default from 'pkg'; Default;",
            "Default",
        ),
        (
            "rust",
            "use pkg::original; fn f(){ let _=original; }",
            "original",
        ),
    ] {
        let at = source.find(needle).unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(at, at + needle.len(), "global pkg original#", 3)],
            vec![],
        );
        let output = join_inventory(
            &inventory,
            &native,
            &sources,
            if language == "rust" { 1 } else { 2 },
        )
        .unwrap();
        assert!(
            output
                .references
                .iter()
                .all(|r| !r.roles.contains(&semantic::Role::Alias)),
            "{language}"
        );
    }
}

#[test]
fn type_role_marks_field_type_not_value_with_same_symbol() {
    let source = "class Base {} class Child extends Base { Base field; void use(){ Base; } }";
    let decl = source.find("Base").unwrap();
    let base = source.find("extends Base").unwrap() + 8;
    let field = source.find("Base field").unwrap();
    let value = source.rfind("Base").unwrap();
    let child = source.find("Child").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        vec![
            (decl, decl + 4, "global pkg Base#", 1),
            (child, child + 5, "global pkg Child#", 1),
            (base, base + 4, "global pkg Base#", 8),
            (field, field + 4, "global pkg Base#", 8),
            (value, value + 4, "global pkg Base#", 8),
        ],
        vec![
            ("global pkg Base#", 7, vec![]),
            (
                "global pkg Child#",
                7,
                vec![("global pkg Base#", true, false)],
            ),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == base && r.roles.contains(&semantic::Role::Type))
    );
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == field && r.roles.contains(&semantic::Role::Type))
    );
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == value && !r.roles.contains(&semantic::Role::Type))
    );
}

#[test]
fn nonexact_invocation_does_not_grant_role_or_binding_even_with_exact_callee() {
    let source = "function f(){ f(); }";
    let at = source.rfind('f').unwrap();
    let (_root, mut native, sources, inventory) = admitted_case_with_calls(
        "javascript",
        source,
        vec![(at, at + 1, "external pkg f.", 8)],
        vec![],
        &[at],
    );
    let call = native
        .calls
        .iter()
        .find(|c| c.callee_range.as_ref().is_some_and(|r| r.start == at))
        .unwrap()
        .clone();
    let mut duplicate = call.clone();
    duplicate.id = native.references[0].id.clone();
    native.calls.push(duplicate);
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Invocation
                && j.status == semantic::JoinStatus::Ambiguous)
    );
    assert!(
        output
            .references
            .iter()
            .all(|r| !r.roles.contains(&semantic::Role::Call))
    );
    assert!(output.call_bindings.is_empty());
}

#[test]
fn javascript_instanceof_is_type_use_but_matching_value_identifier_is_not() {
    let source = "class Base {} function f(value){ return value instanceof Base && Base; }";
    let decl = source.find("Base").unwrap();
    let instance = source.find("instanceof Base").unwrap() + 11;
    let value = source.rfind("Base").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (decl, decl + 4, "typescript pkg Base#", 1),
            (instance, instance + 4, "typescript pkg Base#", 8),
            (value, value + 4, "typescript pkg Base#", 8),
        ],
        vec![("typescript pkg Base#", 7, vec![])],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == instance && r.roles.contains(&semantic::Role::Type))
    );
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == value && !r.roles.contains(&semantic::Role::Type))
    );
}

#[test]
fn imported_name_needs_source_site_besides_authenticated_import_bit() {
    for (language, source, imported, value, encoding) in [
        (
            "java",
            "import pkg.Base; class A { Base other; Base field; }",
            "Base",
            "Base field",
            2,
        ),
        (
            "javascript",
            "import {Base} from 'pkg'; function use(Base){ return Base; }",
            "Base",
            "Base;",
            2,
        ),
        ("python", "import pkg\npkg.value\n", "pkg", "pkg.value", 2),
        (
            "rust",
            "use pkg::Base; fn f(){ let _=Base; }",
            "Base",
            "Base;",
            1,
        ),
    ] {
        let at = source.find(imported).unwrap();
        let use_at = source.rfind(value).unwrap();
        let use_at = use_at + value.find(imported).unwrap();
        let sym = "external pkg Imported#";
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![
                (at, at + imported.len(), sym, 2),
                (use_at, use_at + imported.len(), sym, 2),
            ],
            vec![],
        );
        let output = join_inventory(&inventory, &native, &sources, encoding).unwrap();
        assert!(
            output
                .references
                .iter()
                .any(|r| r.range.start == at && r.roles.contains(&semantic::Role::Import)),
            "{language} import site"
        );
        assert!(
            output
                .references
                .iter()
                .filter(|r| r.range.start == use_at)
                .all(|r| !r.roles.contains(&semantic::Role::Import)),
            "{language} ordinary value"
        );
        if language == "java" {
            assert!(
                output.references.iter().any(|r| r.range.start == use_at),
                "Java ordinary non-import native reference"
            );
        }
    }
}

#[test]
fn annotation_and_explicit_type_contexts_are_not_heritage_only() {
    for (language, source, needle, encoding) in [
        (
            "python",
            "class Base: pass\ndef f(x: Base):\n    return x\n",
            "x: Base",
            2,
        ),
        ("rust", "struct Base; fn f(x: Base) {}", "x: Base", 1),
        (
            "java",
            "class Base {} class A { void f(Base value) {} }",
            "Base value",
            2,
        ),
    ] {
        let at = source.find(needle).unwrap() + needle.find("Base").unwrap();
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(at, at + 4, "external pkg Base#", 8)],
            vec![],
        );
        let output = join_inventory(&inventory, &native, &sources, encoding).unwrap();
        assert!(
            output
                .references
                .iter()
                .any(|r| r.range.start == at && r.roles.contains(&semantic::Role::Type)),
            "{language} explicit type context: {:?}",
            output.references
        );
    }
}
