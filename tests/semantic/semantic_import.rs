#![cfg(feature = "test-support")]
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
    scip_raw::{CapturedScipInventory, ScipCaptureError},
    semantic_batch::testing,
    semantic_evidence as semantic,
    semantic_import::{MeasuredFacts, join_inventory},
};

fn admit(
    capture: &Capture,
    producer: &str,
    language: &str,
    artifact: &[u8],
    cancel: &Arc<AtomicBool>,
) -> CapturedScipInventory {
    let (descriptor, pages) =
        testing::single_artifact_batch(capture, producer, language, artifact).unwrap();
    let pages: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
    let mut inventories =
        testing::admit_in_memory_from_capture(capture, &descriptor, &pages, &[artifact], cancel)
            .unwrap();
    assert_eq!(inventories.len(), 1);
    inventories.remove(0)
}

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
    let options = IndexOptions::new(physical.clone());
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let inventory = admit(
        &capture,
        "scip-typescript",
        "javascript",
        &hex::decode(WIRE).unwrap(),
        &cancel,
    );
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

/// #576: a SCIP artifact names a source Trellis skipped. The producer read the real file, so
/// its text and positions don't match the stored placeholder: the document is admitted with no
/// claims instead of failing the artifact, and the rest of the artifact still imports.
#[test]
fn an_artifact_naming_a_skipped_source_admits_it_without_claims() {
    let text = "function f(){g();}\n";
    for (bytes, cap) in [
        (format!("{text}//{}\n", "x".repeat(600)).into_bytes(), 256),
        ([text.as_bytes(), b"// caf\xe9\n"].concat(), 2 * 1024 * 1024),
    ] {
        let root = tempfile::tempdir().unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::create_dir(physical.join("src")).unwrap();
        fs::write(physical.join("src/A.js"), &bytes).unwrap();
        fs::write(physical.join("src/B.js"), "function g(){}\n").unwrap();
        let mut options = IndexOptions::new(physical.clone());
        options.max_file_bytes = cap;
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        // The real file's text, an occurrence past the placeholder's end, and a readable
        // document beside it.
        let artifact =
            crate::semantic_batch_support::edit_scip(&hex::decode(WIRE).unwrap(), |index| {
                let mut skipped = index.documents[0].clone();
                skipped.text = String::from_utf8_lossy(&bytes).into_owned();
                skipped.occurrences[0].range = vec![2, 0, 3];
                let mut readable = index.documents[0].clone();
                readable.relative_path = "src/B.js".into();
                readable.symbols.clear();
                readable.occurrences.truncate(1);
                readable.occurrences[0].range = vec![0, 9, 10];
                index.documents = vec![skipped, readable];
            });
        let inventory = admit(
            &capture,
            "scip-typescript",
            "javascript",
            &artifact,
            &cancel,
        );
        assert!(!inventory.claims.is_empty());
        assert!(
            inventory
                .claims
                .iter()
                .all(|claim| claim.envelope.relative_path == "src/B.js"),
            "{:?}",
            inventory.claims
        );
        // And the readable document's claims still join its native facts.
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let native =
            native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
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
        let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
        assert!(!facts.joins.is_empty());
    }
}

/// #576 review: a symbol whose only definition is in a skipped source is absent from the
/// artifact's claims, but that proves nothing: its uses stay unresolved, never external. The
/// readable control resolves them into the file.
#[test]
fn a_use_of_a_symbol_defined_only_in_a_skipped_source_is_never_external() {
    let text = "function f(){g();}\n";
    for (bytes, cap, skipped) in [
        (text.as_bytes().to_vec(), 2 * 1024 * 1024, false),
        (
            format!("{text}//{}\n", "x".repeat(600)).into_bytes(),
            256,
            true,
        ),
        (
            [text.as_bytes(), b"// caf\xe9\n"].concat(),
            2 * 1024 * 1024,
            true,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::create_dir(physical.join("src")).unwrap();
        fs::write(physical.join("src/A.js"), &bytes).unwrap();
        fs::write(physical.join("src/B.js"), "A();\n").unwrap();
        let mut options = IndexOptions::new(physical.clone());
        options.max_file_bytes = cap;
        let cancel = Arc::new(AtomicBool::new(false));
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        // B.js only uses `A#`, which A.js defines.
        let artifact =
            crate::semantic_batch_support::edit_scip(&hex::decode(WIRE).unwrap(), |index| {
                let a = index.documents[0].clone();
                let mut b = index.documents[0].clone();
                b.relative_path = "src/B.js".into();
                b.symbols.clear();
                b.occurrences.truncate(1);
                b.occurrences[0].range = vec![0, 0, 1];
                b.occurrences[0].symbol_roles = 0;
                index.documents = vec![a, b];
            });
        let inventory = admit(
            &capture,
            "scip-typescript",
            "javascript",
            &artifact,
            &cancel,
        );
        let identity =
            trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical)
                .unwrap();
        let native =
            native::from_capture(&capture, &physical, &identity.record_id, &cancel).unwrap();
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
        let facts = join_inventory(&inventory, &native, &sources, 2).unwrap();
        assert!(!facts.references.is_empty());
        for resolution in facts
            .references
            .iter()
            .map(|r| &r.resolution)
            .chain(facts.call_bindings.iter().map(|c| &c.resolution))
        {
            if skipped {
                assert_eq!(*resolution, semantic::Resolution::Unresolved, "cap {cap}");
            } else {
                assert_eq!(*resolution, semantic::Resolution::Resolved, "cap {cap}");
            }
        }
    }
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
            .contains("split codepoint")
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
        "producerId":"scip-typescript","artifactHash":inventory.artifact_hashes[0],"sourceSetId":native.source_set.id,
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
    let (path, producer_name, encoding) = match language {
        "java" => ("src/A.java", "scip-java", 2),
        "javascript" => ("src/A.js", "scip-typescript", 2),
        "python" => ("src/A.py", "scip-python", 2),
        "rust" => ("src/A.rs", "rust-analyzer", 1),
        _ => panic!("unsupported test language"),
    };
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
    admitted_artifact_case(language, producer_name, path, source, &bytes, &[])
}

fn admitted_artifact_case(
    language: &str,
    producer_name: &str,
    path: &str,
    source: &str,
    bytes: &[u8],
    files: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    let mut all = vec![(path, source)];
    all.extend_from_slice(files);
    admitted_files_case(language, producer_name, &all, bytes)
}

fn admitted_files_case(
    language: &str,
    producer_name: &str,
    files: &[(&str, &str)],
    bytes: &[u8],
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    let root = tempfile::tempdir().unwrap();
    let physical = fs::canonicalize(root.path()).unwrap();
    for (path, source) in files {
        fs::create_dir_all(physical.join(path).parent().unwrap()).unwrap();
        fs::write(physical.join(path), source).unwrap();
    }
    fs::write(physical.join("artifact.scip"), bytes).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(physical.clone());
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&physical), &physical).unwrap();
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    let inventory = admit(&capture, producer_name, language, bytes, &cancel);
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

// First documents emitted by scip-typescript and scip-python for the producer
// fixtures in issue #420. Preserve the actual marker roles, symbol, absent kind,
// and (for TypeScript) enclosing range instead of inventing a module enum kind.
const JAVASCRIPT_MODULE_DOCUMENT: &str = concat!(
    "0a087372632f612e6a7312500a030000001241736369702d74797065736372697074206e706d207472656c6c69732d70",
    "726f64756365722d666978747572652d6a7320312e302e30207372632f60612e6a73602f18013a040000030012580a03",
    "0010151249736369702d74797065736372697074206e706d207472656c6c69732d70726f64756365722d666978747572",
    "652d6a7320312e302e30207372632f60612e6a73602f677265657428292e18013a040000020112580a0300161a124f73",
    "6369702d74797065736372697074206e706d207472656c6c69732d70726f64756365722d666978747572652d6a732031",
    "2e302e30207372632f60612e6a73602f677265657428292e286e616d6529180112560a03011418124f736369702d7479",
    "7065736372697074206e706d207472656c6c69732d70726f64756365722d666978747572652d6a7320312e302e302073",
    "72632f60612e6a73602f677265657428292e286e616d65291a5c0a41736369702d74797065736372697074206e706d20",
    "7472656c6c69732d70726f64756365722d666978747572652d6a7320312e302e30207372632f60612e6a73602f1a1760",
    "606074730a6d6f64756c652022612e6a73220a6060601a780a49736369702d74797065736372697074206e706d207472",
    "656c6c69732d70726f64756365722d666978747572652d6a7320312e302e30207372632f60612e6a73602f6772656574",
    "28292e1a2b60606074730a66756e6374696f6e206772656574286e616d653a20616e79293a20737472696e670a606060",
    "1a720a4f736369702d74797065736372697074206e706d207472656c6c69732d70726f64756365722d66697874757265",
    "2d6a7320312e302e30207372632f60612e6a73602f677265657428292e286e616d65291a1f60606074730a2870617261",
    "6d6574657229206e616d653a20616e790a606060",
);

const PYTHON_MODULE_DOCUMENT: &str = concat!(
    "0a0f706b672f5f5f696e69745f5f2e7079123d0a030000001234736369702d707974686f6e20707974686f6e20747265",
    "6c6c69732d776f726b7370616365203020706b672f5f5f696e69745f5f3a1801123c0a030000071233736369702d7079",
    "74686f6e20707974686f6e207472656c6c69732d776f726b7370616365203020706b672f56455253494f4e2e18011a44",
    "0a34736369702d707974686f6e20707974686f6e207472656c6c69732d776f726b7370616365203020706b672f5f5f69",
    "6e69745f5f3a1a0c286d6f64756c652920706b671a510a33736369702d707974686f6e20707974686f6e207472656c6c",
    "69732d776f726b7370616365203020706b672f56455253494f4e2e1a1a606060707974686f6e0a6275696c74696e732e",
    "7374720a606060",
);

fn real_module_document(language: &str) -> (scip::types::Document, &'static str, &'static str) {
    use protobuf::Message;
    let (wire, producer, source) = match language {
        "javascript" => (
            JAVASCRIPT_MODULE_DOCUMENT,
            "scip-typescript",
            "export function greet(name) {\n  return \"hello \" + name;\n}\n",
        ),
        "python" => (PYTHON_MODULE_DOCUMENT, "scip-python", "VERSION = \"1\"\n"),
        _ => unreachable!(),
    };
    (
        scip::types::Document::parse_from_bytes(&hex::decode(wire).unwrap()).unwrap(),
        producer,
        source,
    )
}

fn assert_real_module_marker_joins_without_invented_anchor(language: &str) {
    use protobuf::Message;
    let (document, producer, source) = real_module_document(language);
    let path = document.relative_path.clone();
    let marker = &document.occurrences[0];
    assert_eq!(marker.range, vec![0, 0, 0]);
    assert_eq!(marker.symbol_roles, 1);
    assert_eq!(document.symbols[0].kind.value(), 0);
    let marker_symbol = marker.symbol.clone();
    let mut index = scip::types::Index::new();
    index.documents.push(document);
    let bytes = index.write_to_bytes().unwrap();
    // The producer fixture's package root names its module marker (#443).
    let package = [(
        "package.json",
        r#"{"name":"trellis-producer-fixture-js","version":"1.0.0","private":true}"#,
    )];
    let files: &[(&str, &str)] = if language == "javascript" {
        &package
    } else {
        &[]
    };
    let (_root, native, sources, inventory) =
        admitted_artifact_case(language, producer, &path, source, &bytes, files);
    let raw_marker = inventory
        .claims
        .iter()
        .find(|claim| {
            claim.envelope.record_kind == "occurrence"
                && claim.envelope.raw["symbol"] == marker_symbol
        })
        .unwrap();
    assert_eq!(
        raw_marker.envelope.raw["range"],
        json!({"encoding":"legacy","values":[0,0,0]})
    );
    assert_eq!(raw_marker.envelope.raw["symbolRoles"], 1);
    let retained = (
        raw_marker.envelope.raw.clone(),
        raw_marker.canonical_bytes.clone(),
        raw_marker.captured_proof_id.clone(),
    );
    let output = join_inventory(&inventory, &native, &sources, 2)
        .expect("real producer module definition must not abort the complete join");
    assert_eq!(
        retained,
        (
            raw_marker.envelope.raw.clone(),
            raw_marker.canonical_bytes.clone(),
            raw_marker.captured_proof_id.clone(),
        ),
        "joining must retain the captured raw marker and proof"
    );
    let symbol_claim = inventory
        .claims
        .iter()
        .find(|claim| {
            claim.envelope.record_kind == "symbolInformation"
                && claim.envelope.raw["symbol"] == marker_symbol
        })
        .unwrap();
    let symbol = output
        .symbols
        .iter()
        .find(|symbol| symbol.provenance_id == symbol_claim.captured_proof_id)
        .expect("module symbol must be retained");
    assert!(symbol.declarations.is_empty());
    assert!(output.joins.iter().all(|(proof, join)| {
        proof != &raw_marker.captured_proof_id && join.anchor.range.start < join.anchor.range.end
    }));
    assert!(
        output
            .declaration_bindings
            .iter()
            .all(|binding| { binding.provenance_id != raw_marker.captured_proof_id })
    );
    assert!(
        output
            .call_bindings
            .iter()
            .all(|binding| { binding.provenance_id != raw_marker.captured_proof_id })
    );
    assert!(
        output
            .references
            .iter()
            .all(|reference| { reference.provenance_id != raw_marker.captured_proof_id })
    );
    assert!(
        output.declaration_bindings.iter().any(|binding| {
            binding.join.status == semantic::JoinStatus::Exact && binding.syntax_id.is_some()
        }),
        "ordinary declarations in the same emitted document still join"
    );
}

#[test]
fn real_typescript_module_marker_joins_without_invented_anchor() {
    assert_real_module_marker_joins_without_invented_anchor("javascript");
}

#[test]
fn real_python_module_marker_joins_without_invented_anchor() {
    assert_real_module_marker_joins_without_invented_anchor("python");
}

/// Real `@sourcegraph/scip-typescript@0.4.0 index --infer-tsconfig` output for the fixture
/// (issue #444), with the producer's metadata (its absolute project root) dropped and every
/// document kept byte for byte. Like every scip-typescript reference occurrence, the call sites
/// carry no `enclosing_range`.
const ISSUE444_FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/semantic-import/issue444-javascript-calls"
);

fn issue444_case() -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    issue444_case_with_kinds(&[])
}

/// The #444 fixture with the `SymbolInformation.kind` of each symbol ending in a given suffix
/// replaced by an explicit SCIP kind (the real bytes state `0`, unspecified, for every symbol).
fn issue444_case_with_kinds(
    kinds: &[(&str, i32)],
) -> (
    tempfile::TempDir,
    native::Artifact,
    BTreeMap<native::DocumentKey, Vec<u8>>,
    CapturedScipInventory,
) {
    use protobuf::{EnumOrUnknown, Message};
    let read = |path: &str| fs::read_to_string(format!("{ISSUE444_FIXTURE}/{path}")).unwrap();
    let (lib, main) = (read("src/lib.js"), read("src/main.js"));
    let mut bytes = fs::read(format!("{ISSUE444_FIXTURE}/index.scip")).unwrap();
    if !kinds.is_empty() {
        let mut index = scip::types::Index::parse_from_bytes(&bytes).unwrap();
        let mut replaced = 0;
        for info in index
            .documents
            .iter_mut()
            .flat_map(|d| d.symbols.iter_mut())
        {
            assert_eq!(info.kind.value(), 0, "{}", info.symbol);
            if let Some((_, kind)) = kinds.iter().find(|(s, _)| info.symbol.ends_with(s)) {
                info.kind = EnumOrUnknown::from_i32(*kind);
                replaced += 1;
            }
        }
        assert_eq!(replaced, kinds.len());
        bytes = index.write_to_bytes().unwrap();
    }
    admitted_files_case(
        "javascript",
        "scip-typescript",
        &[
            ("src/lib.js", &lib),
            ("src/main.js", &main),
            // The fixture's package root names its module markers (#443).
            (
                "package.json",
                r#"{"name":"issue444-fixture","version":"1.0.0"}"#,
            ),
        ],
        &bytes,
    )
}

/// The byte range of the `nth` occurrence of `needle` followed by `after` in `src/main.js`.
fn issue444_span(needle: &str, after: &str, nth: usize) -> native::Range {
    let main = fs::read_to_string(format!("{ISSUE444_FIXTURE}/src/main.js")).unwrap();
    let (start, _) = main
        .match_indices(&format!("{needle}{after}"))
        .nth(nth)
        .unwrap();
    native::Range {
        start,
        end: start + needle.len(),
    }
}

fn issue444_call<'a>(native: &'a native::Artifact, callee: &native::Range) -> &'a native::Call {
    let calls: Vec<_> = native
        .calls
        .iter()
        .filter(|c| c.document.path == "src/main.js" && c.callee_range.as_ref() == Some(callee))
        .collect();
    assert_eq!(calls.len(), 1, "one measured call at {callee:?}");
    calls[0]
}

#[test]
fn real_scip_typescript_calls_bind_at_exact_measured_callee_spans() {
    let (_root, native, sources, inventory) = issue444_case();
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(
        inventory
            .claims
            .iter()
            .filter(|c| c.envelope.record_kind == "occurrence"
                && c.envelope.raw["symbolRoles"].as_u64() == Some(0))
            .all(|c| c.envelope.raw["enclosingRange"].is_null()),
        "scip-typescript emits no reference enclosing range"
    );
    let reference_at = |range: &native::Range| {
        output
            .references
            .iter()
            .find(|r| r.document.path == "src/main.js" && &r.range == range)
            .unwrap_or_else(|| panic!("reference at {range:?}"))
    };
    let lib_declaration = |name: &str| {
        native
            .declarations
            .iter()
            .find(|d| d.document.path == "src/lib.js" && d.name.as_deref() == Some(name))
            .unwrap()
            .syntax_id
            .clone()
    };
    // Plain call, imported call, constructor, method call, a call whose argument is a
    // callback, and a plain call after a supplementary-plane character (UTF-16 columns).
    // Owner ruling 2026-10-30 (#470): dispatch follows the measured call shape and the bound
    // declaration's kind. Plain calls of free functions, the imported one included, are
    // direct. scip-typescript 0.4.0 gives every symbol information kind 0 (unspecified), which
    // defers to the measured native kind (#473): its constructor (`Widget#<constructor>().`)
    // and method (`Widget#run().`) symbols bind their exact native declarations, so
    // `new Widget(3)` is constructor and the member call `w.run()` resolves with dispatch
    // dynamic (owner ruling 2026-10-31, #557): SCIP names the prototype's declaration.
    let resolved = semantic::Resolution::Resolved;
    let bound = [
        (
            issue444_span("plain", "();\n  helper", 0),
            (resolved, semantic::Dispatch::Direct),
        ),
        (
            issue444_span("helper", "(2)", 0),
            (resolved, semantic::Dispatch::Direct),
        ),
        (
            issue444_span("Widget", "(3)", 0),
            (resolved, semantic::Dispatch::Constructor),
        ),
        (
            issue444_span("run", "()", 0),
            (resolved, semantic::Dispatch::Dynamic),
        ),
        (
            issue444_span("register", "(plain)", 0),
            (resolved, semantic::Dispatch::Direct),
        ),
        (
            issue444_span("plain", "();\n  return", 0),
            (resolved, semantic::Dispatch::Direct),
        ),
    ];
    assert_eq!(output.call_bindings.len(), bound.len());
    for (callee, expected) in &bound {
        let call = issue444_call(&native, callee);
        let bindings: Vec<_> = output
            .call_bindings
            .iter()
            .filter(|b| b.call_id.as_ref() == Some(&call.id))
            .collect();
        assert_eq!(bindings.len(), 1, "one binding for {:?}", call.spelling);
        let binding = bindings[0];
        assert_eq!(binding.join.anchor.kind, semantic::JoinKind::Callee);
        assert_eq!(binding.join.status, semantic::JoinStatus::Exact);
        assert_eq!(&binding.join.anchor.range, callee);
        assert_eq!(binding.join.candidate_ids, vec![call.id.clone()]);
        assert_eq!(
            (binding.resolution, binding.dispatch),
            *expected,
            "{:?}",
            call.spelling
        );
        let reference = reference_at(callee);
        assert!(
            reference.roles.contains(&semantic::Role::Call),
            "{callee:?}"
        );
        assert_eq!(reference.provenance_id, binding.provenance_id);
    }
    let helper = output
        .call_bindings
        .iter()
        .find(|b| b.join.anchor.range == bound[1].0)
        .unwrap();
    assert_eq!(helper.resolution, semantic::Resolution::Resolved);
    assert!(matches!(
        &helper.declared_target,
        Some(semantic::Target::Internal { syntax_id, document, .. })
            if *syntax_id == lib_declaration("helper") && document.path == "src/lib.js"
    ));
    assert_eq!(helper.stale_target, Some(false));
    // The constructor and method calls bind their exact native declarations in `src/lib.js`.
    for (span, target) in [(&bound[2].0, "constructor"), (&bound[3].0, "run")] {
        let binding = output
            .call_bindings
            .iter()
            .find(|b| &b.join.anchor.range == span)
            .unwrap();
        assert!(
            matches!(
                &binding.declared_target,
                Some(semantic::Target::Internal { syntax_id, document, .. })
                    if *syntax_id == lib_declaration(target) && document.path == "src/lib.js"
            ),
            "{target}: {:?}",
            binding.declared_target
        );
        assert_eq!(binding.stale_target, Some(false));
    }
    // A callback argument is a measured reference inside the invocation, not its callee.
    let callback = issue444_span("plain", ");\n  const", 0);
    assert_eq!(reference_at(&callback).roles, vec![semantic::Role::Read]);
    // The receiver of a method call is read, not called.
    let receiver = issue444_span("w", ".run", 0);
    assert_eq!(reference_at(&receiver).roles, vec![semantic::Role::Read]);
    // scip-typescript resolves no symbol for members of an untyped parameter, so these
    // measured calls keep no semantic reference and no binding.
    for (needle, after) in [("method", "()"), ("b", "()")] {
        let call = issue444_call(&native, &issue444_span(needle, after, 0));
        assert!(
            output
                .call_bindings
                .iter()
                .all(|b| b.call_id.as_ref() != Some(&call.id))
        );
        assert!(
            output
                .references
                .iter()
                .all(|r| Some(&r.range) != call.callee_range.as_ref())
        );
    }
}

/// #473: an unspecified SCIP kind (`0`) defers to the measured native declaration kind, but an
/// explicit producer kind is still checked against it. On the real #444 bytes, stating SCIP
/// `Method` (26) for the constructor and `run` agrees with their native `method` declarations.
/// `Constructor` (9) agrees only with the constructor, while `Function` (17) conflicts with
/// both. Plain and imported calls of free functions stay direct in every case.
#[test]
fn real_scip_typescript_explicit_symbol_kind_is_still_checked() {
    use semantic::Dispatch::{Constructor, Direct, Dynamic, Unknown};
    use semantic::Resolution::{Resolved, Unresolved};
    let calls = |kind: i32| {
        let (_root, native, sources, inventory) = issue444_case_with_kinds(&[
            ("Widget#`<constructor>`().", kind),
            ("Widget#run().", kind),
        ]);
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        [
            ("plain", "();\n  helper"),
            ("helper", "(2)"),
            ("Widget", "(3)"),
            ("run", "()"),
            ("register", "(plain)"),
        ]
        .map(|(needle, after)| {
            let span = issue444_span(needle, after, 0);
            let binding = output
                .call_bindings
                .iter()
                .find(|b| b.join.anchor.range == span)
                .unwrap_or_else(|| panic!("binding for {needle}"));
            (needle, binding.resolution, binding.dispatch)
        })
    };
    assert_eq!(
        calls(9),
        [
            ("plain", Resolved, Direct),
            ("helper", Resolved, Direct),
            ("Widget", Resolved, Constructor),
            ("run", Unresolved, Unknown),
            ("register", Resolved, Direct),
        ]
    );
    assert_eq!(
        calls(26),
        [
            ("plain", Resolved, Direct),
            ("helper", Resolved, Direct),
            ("Widget", Resolved, Constructor),
            ("run", Resolved, Dynamic),
            ("register", Resolved, Direct),
        ]
    );
    assert_eq!(
        calls(17),
        [
            ("plain", Resolved, Direct),
            ("helper", Resolved, Direct),
            ("Widget", Unresolved, Unknown),
            ("run", Unresolved, Unknown),
            ("register", Resolved, Direct),
        ]
    );
}

/// A Rust method named `constructor` is still an ordinary method, not the native JavaScript
/// constructor declaration for which explicit SCIP Constructor is compatible.
#[test]
fn rust_method_named_constructor_still_rejects_constructor_kind() {
    let source =
        "struct S;\nimpl S { fn constructor(&self) {} }\nfn f(s: S) { s.constructor(); }\n";
    let symbol = "rust-analyzer cargo c 0.1.0 S#constructor().";
    let definition = source.find("constructor").unwrap();
    let call = source.rfind("constructor").unwrap();
    for (kind, resolution, dispatch) in [
        (
            9,
            semantic::Resolution::Unresolved,
            semantic::Dispatch::Unknown,
        ),
        (
            26,
            semantic::Resolution::Resolved,
            semantic::Dispatch::Direct,
        ),
    ] {
        let (_root, native, sources, inventory) = admitted_case(
            "rust",
            source,
            vec![
                (definition, definition + "constructor".len(), symbol, 1),
                (call, call + "constructor".len(), symbol, 0),
            ],
            vec![(symbol, kind, vec![])],
        );
        let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
        assert_eq!(output.call_bindings.len(), 1);
        let binding = &output.call_bindings[0];
        assert_eq!(binding.resolution, resolution, "explicit kind {kind}");
        assert_eq!(binding.dispatch, dispatch);
    }
}

/// Owner ruling 2026-10-30 (#470): dispatch is the measured native call shape plus the exact
/// bound target's declaration kind. `new` of a class (by identifier or dotted name) is
/// constructor and a plain call of a free function is direct. A class called without `new`,
/// `new` of a function, and member calls (`o.f()`, `o?.f()`) of the same resolved free
/// function stay unknown.
#[test]
fn constructor_call_to_class_is_constructor_dispatch() {
    let source = "class W { constructor() {} }\nfunction f() { return 1; }\n\
        function m(o) { new W(); W(); f(); o.f(); o?.f(); new f(); new o.W(); }\n";
    let at = |needle: &str, inner: usize| source.find(needle).unwrap() + inner;
    let (w, f) = ("pkg W#", "pkg f().");
    let uses = [
        (at("new W(", 4), w, semantic::Dispatch::Constructor),
        (at("; W();", 2), w, semantic::Dispatch::Unknown),
        (at("; f();", 2), f, semantic::Dispatch::Direct),
        (at("o.f(", 2), f, semantic::Dispatch::Unknown),
        (at("o?.f(", 3), f, semantic::Dispatch::Unknown),
        (at("new f(", 4), f, semantic::Dispatch::Unknown),
        (at("o.W(", 2), w, semantic::Dispatch::Constructor),
    ];
    let mut occurrences = vec![
        (at("class W", 6), at("class W", 7), w, 1),
        (at("function f", 9), at("function f", 10), f, 1),
    ];
    occurrences.extend(
        uses.iter()
            .map(|(start, symbol, _)| (*start, start + 1, *symbol, 0)),
    );
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        occurrences,
        vec![(w, 7, vec![]), (f, 17, vec![])],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(output.call_bindings.len(), uses.len());
    for (start, _, dispatch) in uses {
        let binding = output
            .call_bindings
            .iter()
            .find(|b| b.join.anchor.range.start == start)
            .unwrap_or_else(|| panic!("binding at {start}"));
        assert_eq!(binding.join.anchor.kind, semantic::JoinKind::Callee);
        assert_eq!(
            binding.resolution,
            semantic::Resolution::Resolved,
            "{start}"
        );
        assert_eq!(binding.dispatch, dispatch, "{start}");
    }
}

/// Owner rulings 2026-10-30 (#470) and 2026-10-31 (#557) for Rust: identifier, generic and
/// path calls of a free function are direct, and so are method-call syntax and a path call
/// that bind an inherent method or associated function.
#[test]
fn rust_path_plain_and_inherent_calls_are_direct() {
    let source = "mod m { pub fn f() {} }\nstruct S;\nimpl S { fn g(&self) {} fn h() {} }\n\
        fn f2() {}\nfn f3<T>() {}\n\
        fn k(s: S) { m::f(); f2(); crate::m::f(); s.g(); S::h(); f3::<u8>(); }\n";
    let at = |needle: &str, inner: usize| source.find(needle).unwrap() + inner;
    let (f, f2, f3, g, h) = (
        "rust-analyzer cargo c 0.1.0 m/f().",
        "rust-analyzer cargo c 0.1.0 f2().",
        "rust-analyzer cargo c 0.1.0 f3().",
        "rust-analyzer cargo c 0.1.0 S#g().",
        "rust-analyzer cargo c 0.1.0 S#h().",
    );
    let uses = [
        (at("{ m::f(", 5), 1, f, semantic::Dispatch::Direct),
        (at("; f2(", 2), 2, f2, semantic::Dispatch::Direct),
        (at("crate::m::f(", 10), 1, f, semantic::Dispatch::Direct),
        (at("s.g(", 2), 1, g, semantic::Dispatch::Direct),
        (at("S::h(", 3), 1, h, semantic::Dispatch::Direct),
        (at("f3::<", 0), 2, f3, semantic::Dispatch::Direct),
    ];
    let mut occurrences = vec![
        (at("fn f()", 3), at("fn f()", 4), f, 1),
        (at("fn f2", 3), at("fn f2", 5), f2, 1),
        (at("fn f3", 3), at("fn f3", 5), f3, 1),
        (at("fn g", 3), at("fn g", 4), g, 1),
        (at("fn h", 3), at("fn h", 4), h, 1),
    ];
    occurrences.extend(
        uses.iter()
            .map(|(start, width, symbol, _)| (*start, start + width, *symbol, 0)),
    );
    let (_root, native, sources, inventory) = admitted_case(
        "rust",
        source,
        occurrences,
        vec![
            (f, 17, vec![]),
            (f2, 17, vec![]),
            (f3, 17, vec![]),
            (g, 26, vec![]),
            (h, 80, vec![]),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
    assert_eq!(output.call_bindings.len(), uses.len());
    for (start, _, _, dispatch) in uses {
        let binding = output
            .call_bindings
            .iter()
            .find(|b| b.join.anchor.range.start == start)
            .unwrap_or_else(|| panic!("binding at {start}"));
        assert_eq!(
            binding.resolution,
            semantic::Resolution::Resolved,
            "{start}"
        );
        assert_eq!(binding.dispatch, dispatch, "{start}");
    }
}

/// Owner ruling 2026-10-31 (#557): the method's immediate parent decides. An inherent `impl`
/// method is direct through `self.m()`, `x.m()`, `Type::f()` and an allowlisted macro's
/// arguments; a `trait` block method is dynamic, and an `impl Trait for Type` method, which
/// the owner has not ruled on, stays unknown. rust-analyzer states `Method` for a method,
/// `StaticMethod` for an associated function and `TraitMethod` for a bodiless trait method.
#[test]
fn rust_method_dispatch_follows_the_measured_parent() {
    use semantic::Dispatch::{Direct, Dynamic, Unknown};
    let source = "trait T { fn req(&self); fn dflt(&self) {} }
struct S;
        impl S { fn a(&self) { self.b(); } fn b(&self) {} fn c() {} }
        impl T for S { fn req(&self) {} }
        fn k(s: S, d: &dyn T) { s.a(); S::c(); assert!(s.a() == ()); d.req(); T::req(d);         d.dflt(); s.req(); <S as T>::req(&s); }
";
    let at = |needle: &str, inner: usize| source.find(needle).unwrap() + inner;
    let symbol = |name: &str| format!("rust-analyzer cargo c 0.1.0 {name}");
    let (req, dflt, a, b, c, impl_req) = (
        symbol("T#req()."),
        symbol("T#dflt()."),
        symbol("impl#[S]a()."),
        symbol("impl#[S]b()."),
        symbol("impl#[S]c()."),
        symbol("impl#[S][T]req()."),
    );
    let uses = [
        (at("self.b(", 5), &b, Direct),
        (at("s.a();", 2), &a, Direct),
        (at("S::c(", 3), &c, Direct),
        (at("!(s.a(", 4), &a, Direct),
        (at("d.req(", 2), &req, Dynamic),
        (at("T::req(", 3), &req, Dynamic),
        (at("d.dflt(", 2), &dflt, Dynamic),
        (at("s.req(", 2), &impl_req, Unknown),
        (at("<S as T>::req(", 10), &impl_req, Unknown),
    ];
    let width = |symbol: &String| {
        symbol
            .trim_end_matches("().")
            .rsplit([']', '#'])
            .next()
            .unwrap()
            .len()
    };
    let mut occurrences = vec![
        (
            at("fn req(&self);", 3),
            at("fn req(&self);", 6),
            req.as_str(),
            1,
        ),
        (at("fn dflt", 3), at("fn dflt", 7), dflt.as_str(), 1),
        (at("fn a(", 3), at("fn a(", 4), a.as_str(), 1),
        (at("fn b(", 3), at("fn b(", 4), b.as_str(), 1),
        (at("fn c(", 3), at("fn c(", 4), c.as_str(), 1),
        (
            at("fn req(&self) {", 3),
            at("fn req(&self) {", 6),
            impl_req.as_str(),
            1,
        ),
    ];
    occurrences.extend(
        uses.iter()
            .map(|(start, symbol, _)| (*start, start + width(symbol), symbol.as_str(), 0)),
    );
    let (_root, native, sources, inventory) = admitted_case(
        "rust",
        source,
        occurrences,
        vec![
            (req.as_str(), 70, vec![]),
            (dflt.as_str(), 26, vec![]),
            (a.as_str(), 26, vec![]),
            (b.as_str(), 26, vec![]),
            (c.as_str(), 80, vec![]),
            (impl_req.as_str(), 26, vec![]),
        ],
    );
    let bases: Vec<_> = native
        .declarations
        .iter()
        .filter(|d| d.kind == "implementation")
        .map(|d| d.header.bases.clone())
        .collect();
    assert_eq!(bases, [vec![], vec!["T".to_owned()]]);
    let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
    assert_eq!(output.call_bindings.len(), uses.len());
    for (start, _, dispatch) in uses {
        let binding = output
            .call_bindings
            .iter()
            .find(|b| b.join.anchor.range.start == start)
            .unwrap_or_else(|| panic!("binding at {start}"));
        assert_eq!(
            (binding.resolution, binding.dispatch),
            (semantic::Resolution::Resolved, dispatch),
            "{}",
            &source[start..start + 3]
        );
    }
}

/// #557: settlement ambiguity clears a join-time inherent `direct`, as it does a free
/// function's (#470), and a member call with no internal target stays unknown.
#[test]
fn ambiguous_and_external_inherent_calls_stay_unknown() {
    let source = "struct S;
impl S { fn a(&self) {} fn b(&self) {} }
        fn k(s: S) { s.a(); s.b(); }
";
    let at = |needle: &str, inner: usize| source.find(needle).unwrap() + inner;
    let (a, b, std) = (
        "rust-analyzer cargo c 0.1.0 impl#[S]a().",
        "rust-analyzer cargo c 0.1.0 impl#[S]b().",
        "rust-analyzer cargo std 1.98.1 vec/impl#[Vec]b().",
    );
    let use_a = at("s.a(", 2);
    let use_b = at("s.b(", 2);
    let (_root, native, sources, inventory) = admitted_case(
        "rust",
        source,
        vec![
            (at("fn a(", 3), at("fn a(", 4), a, 1),
            (at("fn b(", 3), at("fn b(", 4), b, 1),
            (use_a, use_a + 1, a, 0),
            (use_a, use_a + 1, b, 0),
            (use_b, use_b + 1, std, 0),
        ],
        vec![(a, 26, vec![]), (b, 26, vec![])],
    );
    let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
    let at_use = |start: usize| {
        output
            .call_bindings
            .iter()
            .filter(move |b| b.join.anchor.range.start == start)
    };
    assert!(at_use(use_a).count() == 2);
    assert!(at_use(use_a).all(|b| {
        b.resolution == semantic::Resolution::Ambiguous && b.dispatch == semantic::Dispatch::Unknown
    }));
    assert!(at_use(use_b).all(|b| {
        b.resolution == semantic::Resolution::External && b.dispatch == semantic::Dispatch::Unknown
    }));
}

/// Owner ruling 2026-10-31 (#557): a Java `static`, `private` or `final` method has one body
/// and is direct, measured from `Header.modifiers`; any other method is dynamic.
#[test]
fn java_fixed_methods_are_direct_and_other_methods_dynamic() {
    use semantic::Dispatch::{Direct, Dynamic};
    let source = "class A {
  static void s() {}
  private void p() {}
  final void f() {}
          void v() {}
  void k(A a) { s(); A.s(); p(); this.p(); a.f(); v(); a.v(); }
}
";
    let at = |needle: &str, inner: usize| source.find(needle).unwrap() + inner;
    let symbol = |name: &str| format!("semanticdb maven . . A#{name}().");
    let (s, p, f, v) = (symbol("s"), symbol("p"), symbol("f"), symbol("v"));
    let uses = [
        (at("{ s();", 2), &s, Direct),
        (at("A.s(", 2), &s, Direct),
        (at("; p();", 2), &p, Direct),
        (at("this.p(", 5), &p, Direct),
        (at("a.f(", 2), &f, Direct),
        (at("; v();", 2), &v, Dynamic),
        (at("a.v(", 2), &v, Dynamic),
    ];
    let mut occurrences = vec![
        (at("void s", 5), at("void s", 6), s.as_str(), 1),
        (at("void p", 5), at("void p", 6), p.as_str(), 1),
        (at("void f", 5), at("void f", 6), f.as_str(), 1),
        (at("void v", 5), at("void v", 6), v.as_str(), 1),
    ];
    occurrences.extend(
        uses.iter()
            .map(|(start, symbol, _)| (*start, start + 1, symbol.as_str(), 0)),
    );
    let (_root, native, sources, inventory) = admitted_case(
        "java",
        source,
        occurrences,
        vec![
            (s.as_str(), 80, vec![]),
            (p.as_str(), 26, vec![]),
            (f.as_str(), 26, vec![]),
            (v.as_str(), 26, vec![]),
        ],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(output.call_bindings.len(), uses.len());
    for (start, _, dispatch) in uses {
        let binding = output
            .call_bindings
            .iter()
            .find(|b| b.join.anchor.range.start == start)
            .unwrap_or_else(|| panic!("binding at {start}"));
        assert_eq!(binding.dispatch, dispatch, "{}", &source[start..start + 3]);
    }
}

#[test]
fn module_marker_exception_rejects_other_empty_occurrences() {
    for (language, symbol, roles, kind) in [
        ("javascript", "scip-typescript npm pkg 1.0.0 f().", 1, 17),
        ("javascript", "scip-typescript npm pkg 1.0.0 C#", 1, 7),
        (
            "javascript",
            "scip-typescript npm pkg 1.0.0 src/`A.js`/",
            0,
            0,
        ),
        ("python", "scip-python python pkg 0 f().", 1, 17),
        ("python", "scip-python python pkg 0 C#", 1, 7),
        (
            "python",
            "scip-python python pkg 0 `other.module`/__init__:",
            1,
            0,
        ),
        ("python", "scip-python python pkg 0 `src.A`/__init__:", 8, 0),
    ] {
        let source = if language == "python" {
            "value = 1\n"
        } else {
            "let value = 1;\n"
        };
        let (_root, native, sources, inventory) = admitted_case(
            language,
            source,
            vec![(0, 0, symbol, roles)],
            vec![(symbol, kind, vec![])],
        );
        let error = join_inventory(&inventory, &native, &sources, 2).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<ScipCaptureError>(),
                Some(ScipCaptureError::InvalidArtifact(_))
            ),
            "empty non-module-definition occurrence must fail typed: {language} {symbol} roles={roles}: {error:#}"
        );
    }
}

// Owner ruling 2026-10-29 (#443): admission binds an empty scip-typescript module marker to
// its document under the authenticated package root, so a mismatch rejects the artifact.
#[test]
fn typescript_module_marker_without_its_package_root_rejects_at_admission() {
    use protobuf::Message;
    let fixture = r#"{"name":"trellis-producer-fixture-js","version":"1.0.0"}"#;
    for (case, manifest, symbol) in [
        ("no package root", None, None),
        (
            "other package",
            Some(r#"{"name":"other","version":"1.0.0"}"#),
            None,
        ),
        (
            "other document",
            Some(fixture),
            Some("scip-typescript npm trellis-producer-fixture-js 1.0.0 other/`a.js`/"),
        ),
        (
            "path suffix",
            Some(fixture),
            Some("scip-typescript npm trellis-producer-fixture-js 1.0.0 `a.js`/"),
        ),
    ] {
        let (mut document, _, source) = real_module_document("javascript");
        if let Some(symbol) = symbol {
            document.occurrences[0].symbol = symbol.into();
        }
        let path = document.relative_path.clone();
        let mut index = scip::types::Index::new();
        index.documents.push(document);
        let bytes = index.write_to_bytes().unwrap();
        let root = tempfile::tempdir().unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::create_dir_all(physical.join(&path).parent().unwrap()).unwrap();
        fs::write(physical.join(&path), source).unwrap();
        if let Some(text) = manifest {
            fs::write(physical.join("package.json"), text).unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        let (descriptor, pages) =
            testing::single_artifact_batch(&capture, "scip-typescript", "javascript", &bytes)
                .unwrap();
        let pages: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
        let error = testing::admit_in_memory_from_capture(
            &capture,
            &descriptor,
            &pages,
            &[&bytes],
            &cancel,
        )
        .unwrap_err();
        assert!(
            matches!(error, ScipCaptureError::InvalidArtifact(_)),
            "{case}: {error:?}"
        );
    }
}

#[test]
fn module_marker_exception_rejects_reversed_ranges() {
    use trellis::semantic_import::occurrence_range;
    let (_root, _native, sources, inventory) = fixture();
    let source = sources.values().next().unwrap();
    let mut claim = inventory
        .claims
        .iter()
        .find(|claim| claim.envelope.record_kind == "occurrence")
        .unwrap()
        .clone();
    for symbol in [
        "scip-typescript npm pkg 1.0.0 src/`a.js`/",
        "scip-typescript npm pkg 1.0.0 f().",
        "scip-python python pkg 0 pkg/__init__:",
        "scip-python python pkg 0 f().",
    ] {
        claim.envelope.raw["symbol"] = json!(symbol);
        claim.envelope.raw["range"] = json!({"encoding":"legacy","values":[0,1,0]});
        let error = occurrence_range(&claim, source, 2).unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<ScipCaptureError>(),
                Some(ScipCaptureError::InvalidArtifact(_))
            ),
            "reversed {symbol}: {error:#}"
        );
    }
}

#[test]
fn same_target_reference_roles_union_only_validated_claim_roles() {
    let source = "function useIt(){ value; }\n";
    let start = source.find("value").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (start, start + 5, "external pkg value.", 0),
            (start, start + 5, "external pkg value.", 4),
            // An import flag at a non-import site must not add the Import role.
            (start, start + 5, "external pkg value.", 2),
        ],
        vec![],
    );
    let joined = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(joined.references.len(), 1);
    let reference = &joined.references[0];
    assert_eq!(
        reference.roles,
        vec![semantic::Role::Read, semantic::Role::Write]
    );
    assert_eq!(reference.resolution, semantic::Resolution::External);
    assert!(reference.candidates.is_empty());
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
        // SCIP has no call role: a use occurrence whose exact span is one measured callee
        // names that call (#444), with no enclosing range required.
        assert!(
            reference.roles.contains(&semantic::Role::Call),
            "{language} exact callee use is a call"
        );
        let call = native
            .calls
            .iter()
            .find(|c| c.callee_range.as_ref() == Some(&reference.range))
            .unwrap();
        assert_eq!(output.call_bindings.len(), 1, "{language}");
        let binding = &output.call_bindings[0];
        assert_eq!(binding.call_id.as_ref(), Some(&call.id), "{language}");
        assert_eq!(binding.join.anchor.kind, semantic::JoinKind::Callee);
        assert_eq!(binding.join.status, semantic::JoinStatus::Exact);
        assert_eq!(binding.resolution, semantic::Resolution::External);
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
fn exact_callee_use_binds_with_or_without_matching_enclosing_range() {
    let source = "function f(){ f(); }";
    let offset = source.rfind('f').unwrap();
    // No enclosing range, as scip-typescript, scip-java and scip-python emit (#444), and a
    // producer enclosing range that joins exactly to the same measured invocation.
    for enclosing in [&[][..], &[offset][..]] {
        let (_root, native, sources, inventory) = admitted_case_with_calls(
            "javascript",
            source,
            vec![(offset, offset + 1, "external pkg f.", 0)],
            vec![],
            enclosing,
        );
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        assert_eq!(output.call_bindings.len(), 1, "{enclosing:?}");
        let binding = &output.call_bindings[0];
        assert_eq!(binding.join.anchor.kind, semantic::JoinKind::Callee);
        assert_eq!(binding.join.status, semantic::JoinStatus::Exact);
        assert_eq!(binding.call_id, Some(native.calls[0].id.clone()));
        assert_eq!(binding.resolution, semantic::Resolution::External);
        assert!(output.references[0].roles.contains(&semantic::Role::Call));
    }
    // A use inside the invocation that is not its measured callee is never a call.
    let source = "function f(){ g(f); }";
    let offset = source.rfind('f').unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![(offset, offset + 1, "external pkg f.", 0)],
        vec![],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert!(output.call_bindings.is_empty());
    assert_eq!(output.references[0].roles, vec![semantic::Role::Read]);
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Callee
                && j.status == semantic::JoinStatus::Unmatched)
    );
    assert!(!native.calls.is_empty());
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

/// Each claim alone is a plain call of a resolved free function, classified direct at join
/// time; settlement makes the call ambiguous over both, so neither row stays direct (#470).
#[test]
fn settlement_ambiguity_clears_join_time_direct_dispatch() {
    let source = "function f(){ g(); }\nfunction g(){}\nfunction h(){}\n";
    let at = source.find("g(); ").unwrap();
    let (g, h) = (source.find("g(){}").unwrap(), source.find("h(){}").unwrap());
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![
            (g, g + 1, "pkg g().", 1),
            (h, h + 1, "pkg h().", 1),
            (at, at + 1, "pkg g().", 0),
            (at, at + 1, "pkg h().", 0),
        ],
        vec![("pkg g().", 17, vec![]), ("pkg h().", 17, vec![])],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(output.call_bindings.len(), 2);
    assert!(output.call_bindings.iter().all(|b| {
        b.resolution == semantic::Resolution::Ambiguous
            && b.candidates.len() == 2
            && b.dispatch == semantic::Dispatch::Unknown
    }));
}

/// Owner ruling 4 (2026-10-30, #394): a producer enclosing range that encloses the use but
/// joins no invocation exactly is treated as absent, and the exact callee span decides. One
/// that exactly joins a different invocation than the callee's still binds no call (#453).
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
    // The callee token alone: it encloses the use and matches no measured invocation.
    raw.envelope.raw["enclosingRange"] = json!({"encoding":"legacy","values":[0,at,at+1]});
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    assert_eq!(output.call_bindings.len(), 1);
    let binding = &output.call_bindings[0];
    assert_eq!(binding.join.anchor.kind, semantic::JoinKind::Callee);
    assert_eq!(binding.call_id, Some(native.calls[0].id.clone()));
    assert!(output.references[0].roles.contains(&semantic::Role::Call));
    assert!(
        output
            .joins
            .iter()
            .all(|(_, j)| j.anchor.kind != semantic::JoinKind::Invocation),
        "an absent enclosing range authors no invocation join"
    );

    // `f` is the callee of the inner call; its range names the outer invocation exactly.
    let source = "function f(){ g(f()); }";
    let at = source.rfind("f(").unwrap();
    let outer = source.find("g(").unwrap();
    let close = source.rfind(')').unwrap() + 1;
    let (_root, native, sources, mut inventory) = admitted_case(
        "javascript",
        source,
        vec![(at, at + 1, "external pkg f.", 8)],
        vec![],
    );
    let raw = inventory
        .claims
        .iter_mut()
        .find(|c| c.envelope.record_kind == "occurrence")
        .unwrap();
    raw.envelope.raw["enclosingRange"] = json!({"encoding":"legacy","values":[0,outer,close]});
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
                && j.status == semantic::JoinStatus::Exact)
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
    for at in [base, field] {
        let reference = output
            .references
            .iter()
            .find(|r| r.range.start == at)
            .expect("measured Java type reference");
        assert_eq!(
            reference.roles,
            vec![semantic::Role::Type],
            "Java type-only site {at}"
        );
    }
    assert!(
        output
            .references
            .iter()
            .any(|r| r.range.start == value && r.roles == vec![semantic::Role::Read])
    );
}

/// Two measured calls share the span: the invocation join is ambiguous, so the enclosing range
/// is absent (owner ruling 4, 2026-10-30, #394), and the ambiguous callee join binds nothing.
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
            .all(|(_, j)| j.anchor.kind != semantic::JoinKind::Invocation)
    );
    assert!(
        output
            .joins
            .iter()
            .any(|(_, j)| j.anchor.kind == semantic::JoinKind::Callee
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
                .any(|r| r.range.start == at && r.roles == vec![semantic::Role::Type]),
            "{language} explicit type-only context: {:?}",
            output.references
        );
    }
}

#[test]
fn nested_javascript_runtime_values_inside_type_expression_are_not_types() {
    for source in [
        "class Base {} function getConstructor(arg){ return Base; } function use(value,arg){ return value instanceof getConstructor(arg); }",
        "class Base {} function mixin(value){ return Base; } class Child extends mixin(value) {}",
    ] {
        let direct = source.find("class Base").unwrap() + 6;
        let nested = source
            .rfind("(arg)")
            .or_else(|| source.rfind("(value)"))
            .unwrap()
            + 1;
        let nested_len = if &source[nested..nested + 3] == "arg" {
            3
        } else {
            5
        };
        let mut claims = vec![
            (direct, direct + 4, "typescript pkg Base#", 1),
            (nested, nested + nested_len, "external pkg value.", 8),
        ];
        if source.contains("class Child") {
            let child = source.find("class Child").unwrap() + 6;
            claims.push((child, child + 5, "typescript pkg Child#", 1));
        }
        let (_root, native, sources, inventory) = admitted_case(
            "javascript",
            source,
            claims,
            vec![("typescript pkg Base#", 7, vec![])],
        );
        let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
        let reference = output
            .references
            .iter()
            .find(|r| r.range.start == nested)
            .expect("native measured nested value reference");
        assert_eq!(
            reference.roles,
            vec![semantic::Role::Read],
            "nested runtime argument remains a value"
        );
    }
    let source = "class Base {} function use(value){ return value instanceof Base; }";
    let at = source.rfind("Base").unwrap();
    let (_root, native, sources, inventory) = admitted_case(
        "javascript",
        source,
        vec![(at, at + 4, "typescript pkg Base#", 8)],
        vec![],
    );
    let output = join_inventory(&inventory, &native, &sources, 2).unwrap();
    let reference = output
        .references
        .iter()
        .find(|r| r.range.start == at)
        .expect("native direct class value");
    assert_eq!(
        reference.roles,
        vec![semantic::Role::Read, semantic::Role::Type]
    );
}

mod batch_admission {
    use super::*;
    use serde_json::Value;
    use trellis::{
        scip_raw::{
            AuthenticatedArtifact, CapturedClaim, DocumentSource, ScipCaptureError, VecSink,
            bind_document_language, decode_streaming,
        },
        semantic_batch::{BatchCounters, BatchLimits, descriptor::Descriptor},
    };

    struct World {
        _root: tempfile::TempDir,
        physical: std::path::PathBuf,
        capture: Capture,
        artifact: Vec<u8>,
        cancel: Arc<AtomicBool>,
    }

    /// Three JavaScript documents covered by one artifact.
    fn world() -> World {
        world_with_language("javascript")
    }

    fn world_with_language(language: &str) -> World {
        use protobuf::{EnumOrUnknown, Message};
        let root = tempfile::tempdir().unwrap();
        let physical = fs::canonicalize(root.path()).unwrap();
        fs::create_dir(physical.join("src")).unwrap();
        let mut index = scip::types::Index::new();
        for (path, name) in [("src/A.js", "a"), ("src/B.js", "b"), ("src/C.js", "c")] {
            fs::write(physical.join(path), format!("function {name}(){{}}\n")).unwrap();
            let mut document = scip::types::Document::new();
            document.language = language.into();
            document.relative_path = path.into();
            document.position_encoding = EnumOrUnknown::from_i32(2);
            let mut occurrence = scip::types::Occurrence::new();
            occurrence.range = vec![0, 9, 10];
            occurrence.symbol = format!("pkg {name}.");
            occurrence.symbol_roles = 1;
            document.occurrences.push(occurrence);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = format!("pkg {name}.");
            info.kind = EnumOrUnknown::from_i32(17);
            info.display_name = name.into();
            document.symbols.push(info);
            index.documents.push(document);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let capture =
            Capture::admit(&IndexOptions::new(physical.clone()), &cancel, &|_| {}).unwrap();
        World {
            _root: root,
            physical,
            capture,
            artifact: index.write_to_bytes().unwrap(),
            cancel,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(bytes))
    }

    type Batch = (Value, Vec<Value>);
    type HeaderEdit = fn(&mut Value);

    fn batch(world: &World) -> Batch {
        let (descriptor, pages) = testing::single_artifact_batch(
            &world.capture,
            "scip-typescript",
            "javascript",
            &world.artifact,
        )
        .unwrap();
        (
            serde_json::from_slice(&descriptor).unwrap(),
            pages
                .iter()
                .map(|page| serde_json::from_slice(page).unwrap())
                .collect(),
        )
    }

    /// Admits `batch` with the entries' lengths and digests re-derived from the page values, so
    /// an edit exercises the rule under test rather than the page digest.
    fn admit_batch(
        world: &World,
        (mut descriptor, pages): Batch,
        artifacts: &[&[u8]],
    ) -> Result<Vec<trellis::scip_raw::CapturedScipInventory>, String> {
        let bytes: Vec<Vec<u8>> = pages
            .iter()
            .map(|page| serde_json::to_vec(page).unwrap())
            .collect();
        for (entry, page) in descriptor["pages"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(&bytes)
        {
            entry["byteLength"] = page.len().into();
            entry["sha256"] = sha(page).into();
        }
        descriptor["totals"]["pageBytes"] = bytes.iter().map(Vec::len).sum::<usize>().into();
        let refs: Vec<&[u8]> = bytes.iter().map(Vec::as_slice).collect();
        testing::admit_in_memory_from_capture(
            &world.capture,
            &serde_json::to_vec(&descriptor).unwrap(),
            &refs,
            artifacts,
            &world.cancel,
        )
        .map_err(|error| match error {
            ScipCaptureError::InvalidAdmission(detail) => detail,
            other => format!("{other:?}"),
        })
    }

    fn detail(world: &World, batch: Batch) -> String {
        admit_batch(world, batch, &[&world.artifact]).expect_err("admission must fail")
    }

    #[test]
    fn single_artifact_streaming_decoder_matches_batch_admission_for_document_languages() {
        for language in ["javascript", "", "python"] {
            let world = world_with_language(language);
            let (descriptor, pages) = testing::single_artifact_batch(
                &world.capture,
                "scip-typescript",
                "javascript",
                &world.artifact,
            )
            .unwrap();
            let pages: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
            let admitted = testing::admit_in_memory_from_capture(
                &world.capture,
                &descriptor,
                &pages,
                &[&world.artifact],
                &world.cancel,
            );
            let pinned = trellis::scip_admission::pinned_producer_by_id("scip-typescript").unwrap();
            let hash = sha(&world.artifact);
            let uri = format!("file://{}", world.physical.display());
            let mut sink = VecSink::default();
            let mut resolve = |path: &str, raw_language: &str| {
                let file = world.capture.files.iter().find(|f| f.path == path).unwrap();
                let language =
                    bind_document_language(raw_language, &file.language, &pinned.languages)?;
                Ok(DocumentSource {
                    language: language.to_owned(),
                    bytes: std::borrow::Cow::Owned(file.text.as_bytes().to_vec()),
                    content_hash: file.hash.clone(),
                })
            };
            let decoded = decode_streaming(
                world.artifact.as_slice(),
                world.artifact.len() as u64,
                &AuthenticatedArtifact {
                    artifact_hash: &hash,
                    producer_id: "scip-typescript",
                    producer_version: &admitted_version(),
                    source_set_id: &admitted_source_set(&world),
                    source_revision: &admitted_revision(&world),
                    position_encoding: 2,
                    protocol_version: 0,
                    text_document_encoding: 1,
                    project_root_uri: &uri,
                    module_roots: &trellis::scip_raw::ModuleRoots::default(),
                    cargo_targets: &trellis::scip_raw::CargoTargets::default(),
                    assigned_documents: 3,
                },
                &mut resolve,
                &mut BatchCounters::default(),
                &BatchLimits::PRODUCTION,
                &mut sink,
                &world.cancel,
            )
            .and_then(|_| sink.finish());
            if language == "python" {
                for error in [admitted.unwrap_err(), decoded.unwrap_err()] {
                    assert!(
                        matches!(error, ScipCaptureError::InvalidArtifact(ref detail)
                            if detail == "document source binding"),
                        "{language}: {error:?}"
                    );
                }
                continue;
            }
            let inventories = admitted.unwrap();
            assert_eq!(inventories.len(), 1);
            let admitted = &inventories[0];
            let direct: Vec<CapturedClaim> = decoded.unwrap();
            assert_eq!(admitted.claims.len(), direct.len());
            assert!(!direct.is_empty());
            assert!(direct.iter().all(|claim| claim.language == "javascript"));
            let key = |claim: &CapturedClaim| {
                (
                    claim.envelope.relative_path.clone(),
                    claim.canonical_bytes.clone(),
                )
            };
            let mut left: Vec<_> = admitted.claims.iter().collect();
            let mut right: Vec<_> = direct.iter().collect();
            left.sort_by_key(|claim| key(claim));
            right.sort_by_key(|claim| key(claim));
            for (a, b) in left.into_iter().zip(right) {
                assert_eq!(a, b, "raw language {language:?}");
            }
            assert_eq!(admitted.artifact_hashes, vec![hash]);
        }
    }

    fn header(world: &World) -> trellis::native_evidence::Artifact {
        let identity = trellis::store::topology::WorkspaceIdentity::discover(
            Some(&world.physical),
            &world.physical,
        )
        .unwrap();
        native::from_capture(
            &world.capture,
            &world.physical,
            &identity.record_id,
            &world.cancel,
        )
        .unwrap()
    }
    fn admitted_source_set(world: &World) -> String {
        header(world).source_set.id
    }
    fn admitted_revision(world: &World) -> String {
        header(world).revision.id
    }
    fn admitted_version() -> String {
        let profile: Value = serde_json::from_str(include_str!(
            "../../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
        ))
        .unwrap();
        profile["semanticProducers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "scip-typescript")
            .unwrap()["version"]
            .as_str()
            .unwrap()
            .into()
    }

    #[test]
    fn header_binding_faults_fail_typed_through_admission() {
        let world = world();
        let cases: [(&str, HeaderEdit); 4] = [
            ("native revision components", |d| {
                d["sourceRevision"] = "x".into()
            }),
            ("native revision components", |d| {
                d["components"]["configHash"] = "0".repeat(64).into()
            }),
            ("metadata binding", |d| {
                d["metadata"]["projectRoot"] = "file:///elsewhere".into()
            }),
            ("source manifest hash", |d| {
                d["sourceManifestHash"] = "not-a-hash".into()
            }),
        ];
        for (expected, edit) in cases {
            let (mut descriptor, pages) = batch(&world);
            edit(&mut descriptor);
            assert_eq!(detail(&world, (descriptor, pages)), expected);
        }
    }

    #[test]
    fn page_and_artifact_entry_faults_fail_typed_through_admission() {
        let world = world();
        let (descriptor, pages) = batch(&world);
        let mut repeated = (descriptor.clone(), pages.clone());
        repeated.0["pages"]
            .as_array_mut()
            .unwrap()
            .insert(1, descriptor["pages"][0].clone());
        repeated.1.insert(1, pages[0].clone());
        assert_eq!(detail(&world, repeated), "page ordinals");
        let mut missing = (descriptor.clone(), pages.clone());
        missing.0["pages"].as_array_mut().unwrap().remove(1);
        missing.1.remove(1);
        assert_eq!(detail(&world, missing), "page ordinals");
        let mut duplicate = descriptor.clone();
        let mut second = duplicate["artifacts"][0].clone();
        second["ordinal"] = 1.into();
        second["sha256"] = sha(b"another artifact").into();
        duplicate["artifacts"].as_array_mut().unwrap().push(second);
        duplicate["totals"]["artifactBytes"] =
            (duplicate["totals"]["artifactBytes"].as_u64().unwrap() * 2).into();
        assert_eq!(detail(&world, (duplicate, pages)), "duplicate artifact");
    }

    #[test]
    fn source_inventory_faults_fail_typed_through_admission() {
        let world = world();
        let (mut descriptor, pages) = batch(&world);
        descriptor["sourceManifestHash"] = "0".repeat(64).into();
        assert_eq!(detail(&world, (descriptor, pages)), "source manifest hash");

        let native = header(&world);
        let sources: BTreeMap<_, _> = world
            .capture
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
        let (descriptor, pages) = testing::single_artifact_batch(
            &world.capture,
            "scip-typescript",
            "javascript",
            &world.artifact,
        )
        .unwrap();
        let page_refs: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
        let admit_with = |native: &native::Artifact| {
            testing::admit_in_memory(
                &world.physical,
                native,
                &sources,
                &testing::captured_native_config(&world.capture).unwrap(),
                &descriptor,
                &page_refs,
                &[&world.artifact],
                &world.cancel,
            )
            .unwrap_err()
        };
        let mut fewer = native.clone();
        fewer.revision.documents.remove(0);
        assert_eq!(
            admit_with(&fewer),
            ScipCaptureError::InvalidAdmission("extra captured source".into())
        );
        let mut more = native.clone();
        let mut phantom = more.revision.documents[1].clone();
        phantom.key.path = "src/C.js".into();
        more.revision.documents.push(phantom);
        assert_eq!(
            admit_with(&more),
            ScipCaptureError::InvalidAdmission("missing captured source".into())
        );
    }

    #[test]
    fn a_coverage_page_that_omits_a_requested_document_fails_completion() {
        let world = world();
        let (mut descriptor, mut pages) = batch(&world);
        pages[1]["rows"].as_array_mut().unwrap().pop();
        descriptor["pages"][1]["rowCount"] = 2.into();
        descriptor["totals"]["coveredDocuments"] = 2.into();
        assert_eq!(detail(&world, (descriptor, pages)), "coverage completion");
    }

    #[test]
    fn excluded_documents_attach_to_one_artifact_of_their_language() {
        use protobuf::Message;
        let world = world();
        let (mut descriptor, mut pages) = batch(&world);
        let index = scip::types::Index::parse_from_bytes(&world.artifact).unwrap();
        let split = |documents: &[usize]| {
            let mut part = index.clone();
            part.documents = documents
                .iter()
                .map(|d| index.documents[*d].clone())
                .collect();
            part.write_to_bytes().unwrap()
        };
        let (first, second) = (split(&[0]), split(&[1]));
        let entry = descriptor["artifacts"][0].clone();
        descriptor["artifacts"] = serde_json::json!([entry.clone(), entry]);
        descriptor["artifacts"][1]["ordinal"] = 1.into();
        descriptor["artifacts"][1]["relativePath"] = "artifact-1.scip".into();
        for (ordinal, bytes) in [&first, &second].into_iter().enumerate() {
            descriptor["artifacts"][ordinal]["byteLength"] = bytes.len().into();
            descriptor["artifacts"][ordinal]["sha256"] = sha(bytes).into();
        }
        descriptor["totals"]["artifactBytes"] = (first.len() + second.len()).into();
        pages[1]["rows"][1]["artifact"] = 1.into();
        pages[1]["rows"][2]["artifact"] = Value::Null;
        pages[1]["rows"][2]["exclusion"] = "unsupported".into();
        descriptor["completion"] = "partial".into();
        descriptor["totals"]["coveredDocuments"] = 2.into();
        descriptor["totals"]["excludedDocuments"] = 1.into();
        let inventories = admit_batch(&world, (descriptor, pages), &[&first, &second]).unwrap();
        let excluded: Vec<_> = inventories
            .iter()
            .map(|inventory| {
                inventory
                    .exclusions
                    .iter()
                    .map(|e| e.relative_path.as_str())
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(excluded, vec![vec!["src/C.js"], vec![]]);
    }

    #[test]
    fn role_paths_must_exist_under_the_root_without_symlinks() {
        let world = world();
        let (descriptor, pages) = batch(&world);
        let parsed = Descriptor::parse(&serde_json::to_vec(&descriptor).unwrap()).unwrap();
        assert_eq!(
            parsed.check_paths(&world.physical),
            Err(ScipCaptureError::InvalidAdmission(
                "selected role absent".into()
            ))
        );
        for (name, page) in ["source-00000.json", "coverage-00000.json"]
            .iter()
            .zip(&pages)
        {
            fs::write(world.physical.join(name), serde_json::to_vec(page).unwrap()).unwrap();
        }
        fs::write(world.physical.join("artifact.scip"), &world.artifact).unwrap();
        parsed.check_paths(&world.physical).unwrap();

        fs::remove_file(world.physical.join("artifact.scip")).unwrap();
        std::os::unix::fs::symlink(
            world.physical.join("src/A.js"),
            world.physical.join("artifact.scip"),
        )
        .unwrap();
        assert_eq!(
            parsed.check_paths(&world.physical),
            Err(ScipCaptureError::InvalidAdmission(
                "symlinked selected role".into()
            ))
        );
        let mut escaping = descriptor.clone();
        escaping["artifacts"][0]["relativePath"] = "../artifact.scip".into();
        let escaping = Descriptor::parse(&serde_json::to_vec(&escaping).unwrap()).unwrap();
        assert_eq!(
            escaping.check_paths(&world.physical),
            Err(ScipCaptureError::InvalidAdmission(
                "invalid role path".into()
            ))
        );
    }
}

fn joined_roles(source: &str, at: usize, length: usize, roles: i32) -> Vec<semantic::Role> {
    let occurrence = (at, at + length, "external pkg Measured#", roles);
    let (_root, native, sources, inventory) =
        admitted_case("rust", source, vec![occurrence], vec![]);
    let output = join_inventory(&inventory, &native, &sources, 1).unwrap();
    let reference = output.references.iter().find(|r| r.range.start == at);
    reference
        .unwrap_or_else(|| panic!("no reference"))
        .roles
        .clone()
}

/// Measured macro contents have role sites at their document spans (#551).
#[test]
fn explicit_types_and_imports_inside_measured_macros_keep_their_roles() {
    let source = "struct Item; struct Total;\nfn f(items: &[Item]) {\n    assert!(items.iter().all(|v: &Item| {\n        \
                  use std::fmt::Debug;\n        let total: Total = Total;\n        \
                  format!(\"{}\", (|w: Item| 1)(Item)).len() > 0\n    }));\n}\n";
    let last = |name: &str| source.rfind(name).unwrap();
    let closure = source.find("|v: &Item").unwrap() + "|v: &".len();
    assert_eq!(joined_roles(source, closure, 4, 8), [semantic::Role::Type]);
    let local = source.find("total: Total").unwrap() + "total: ".len();
    assert_eq!(joined_roles(source, local, 5, 8), [semantic::Role::Type]);
    let nested = source.find("|w: Item").unwrap() + "|w: ".len();
    assert_eq!(joined_roles(source, nested, 4, 8), [semantic::Role::Type]);
    assert!(joined_roles(source, last("Debug"), 5, 2).contains(&semantic::Role::Import));
}

/// A macro measurement left opaque must not use up the expansion budget of a later measured
/// macro: its type would join as a plain read, which is a wrong role, not an omission.
#[test]
fn a_macro_measurement_refused_cannot_starve_a_later_measured_macro_of_its_roles() {
    // Compiles as is. Built here because a file this deep fails the Architecture of a checkout
    // holding it, under the reference walk's own depth bound.
    let source = format!(
        "#![recursion_limit=\"2048\"]\nstruct LaterType; fn f() {{ let value = LaterType; \
         assert!({}true{}); assert!({{ let x: LaterType = value; true }}, \"{}\"); }}{}",
        "dbg!(".repeat(400),
        ")".repeat(400),
        "x".repeat(5000),
        "",
    );
    let at = source.rfind("LaterType").unwrap();
    assert_eq!(joined_roles(&source, at, 9, 8), [semantic::Role::Type]);
}
