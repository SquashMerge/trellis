use super::*;

fn producer(version: &str) -> Producer {
    Producer {
        id: PRODUCER.into(),
        version: version.into(),
        executable_hash: "e".repeat(64),
        kind: "native".into(),
        languages: LANGUAGES.map(str::to_owned).into(),
        position_encoding: "utf8".into(),
    }
}
fn selected(file: &SourceFile, revision_id: &str, version: &str) -> Result<Artifact> {
    let source_set = SourceSet {
        id: "source-set:v1:root".into(),
        root_id: "root".into(),
        languages: LANGUAGES.map(str::to_owned).into(),
        dependencies: vec![],
    };
    let key = DocumentKey {
        source_set_id: source_set.id.clone(),
        language: file.language.clone(),
        path: file.path.clone(),
    };
    let revision = Revision {
        id: revision_id.into(),
        source_set_id: source_set.id.clone(),
        documents: vec![Document {
            key,
            revision_id: revision_id.into(),
            content_hash: file.hash.clone(),
            byte_length: file.text.len(),
        }],
        toolchain_hash: "1".repeat(64),
        config_hash: hash(revision_id.as_bytes()),
        dependency_hash: "3".repeat(64),
    };
    let mut artifact = Artifact {
        producer: producer(version),
        source_set,
        revision,
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
        references: vec![],
        capability_reports: vec![],
        search_cards: Vec::new(),
    };
    let document = artifact.revision.documents[0].clone();
    extract_known(
        &mut artifact,
        file,
        &document,
        &mut IdentityRegistry::default(),
    )?;
    Ok(artifact)
}
fn file(text: &str) -> SourceFile {
    SourceFile {
        path: "a.js".into(),
        hash: hash(text.as_bytes()),
        language: "javascript".into(),
        text: text.into(),
    }
}

fn check_inventory_registry(rust_inventory: &Value, semantic_inventory: &Value) -> Result<()> {
    ensure!(
        rust_inventory == semantic_inventory,
        "Rust and semantic-contract native extraction inventories differ"
    );
    Ok(())
}

#[test]
fn native_extraction_inventory_matches_semantic_contract_registry() {
    let rust_inventory = json!(EXTRACTION_INPUTS);
    let output = std::process::Command::new("node")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args([
            "--input-type=module",
            "-e",
            r#"
import { productionInventoryEntries } from './tools/semantic-contract/native-inventory.mjs';
console.log(JSON.stringify(productionInventoryEntries()));
"#,
        ])
        .output()
        .expect("Node is required by the repository verification suite");
    assert!(
        output.status.success(),
        "semantic inventory lookup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let semantic_inventory: Value = serde_json::from_slice(&output.stdout).unwrap();
    check_inventory_registry(&rust_inventory, &semantic_inventory).unwrap();
    // A stale JS version must not disappear through Rust-driven lookups.
    let mut extra_js_version = semantic_inventory.clone();
    let mut stale = extra_js_version[0].clone();
    stale[1] = json!("native-v3");
    extra_js_version.as_array_mut().unwrap().push(stale);
    assert!(check_inventory_registry(&rust_inventory, &extra_js_version).is_err());
    // The same comparison must reject drift on either side, not just a missing entry.
    for change_rust in [false, true] {
        for change_version in [false, true] {
            let mut rust = rust_inventory.clone();
            let mut semantic = semantic_inventory.clone();
            let changed = if change_rust {
                &mut rust
            } else {
                &mut semantic
            };
            if change_version {
                changed[0][1] = json!("native-unregistered-future");
            } else {
                changed[0][2][0][1] = json!(["config"]);
            }
            assert!(
                check_inventory_registry(&rust, &semantic).is_err(),
                "accepted drift: Rust={change_rust}, version={change_version}"
            );
        }
    }
}

#[test]
fn native_producer_declares_and_authenticates_an_explicit_empty_inventory() {
    let revision = selected(&file("f();"), "revision:v2:r1", NATIVE_VERSION)
        .unwrap()
        .revision;
    for language in LANGUAGES {
        let measured =
            native_extraction_context(&producer(NATIVE_VERSION), language, &revision).unwrap();
        let reusable =
            declared_selected_extraction_context(PRODUCER, NATIVE_VERSION, language).unwrap();
        assert_eq!(
            measured, reusable,
            "witness and selected reuse used different declared inventory"
        );
        assert_eq!(
            measured,
            extraction_context(language, &[]).unwrap(),
            "current empty-inventory occ:v2 identity changed"
        );
        assert!(
            selected_extraction_context_for_inventory(language, &["config"]).is_err(),
            "synthetic nonempty declaration must not reuse an unauthenticated empty context"
        );
    }
    // An undeclared producer version (e.g. the withdrawn occ:v1 producer) or language has
    // no authenticated inventory, so it fails closed.
    for version in ["native-v2", "native-v3"] {
        assert!(native_extraction_context(&producer(version), "rust", &revision).is_err());
    }
    assert!(native_extraction_context(&producer(NATIVE_VERSION), "go", &revision).is_err());
    let mut captured = revision.clone();
    captured.config_hash = hash(b"captured config");
    assert_eq!(
        authenticated_extraction_context(
            "rust",
            &["config"],
            &[("config", Some(b"captured config"))],
            &captured,
        )
        .unwrap(),
        extraction_context("rust", &[("config".into(), captured.config_hash.clone())]).unwrap()
    );
    for (inventory, observed) in [
        (vec!["config"], vec![]),
        (vec!["config"], vec![("config", None)]),
        (
            vec!["config"],
            vec![("config", Some(b"stale config".as_slice()))],
        ),
        (
            vec![],
            vec![("config", Some(b"captured config".as_slice()))],
        ),
        (
            vec!["settings"],
            vec![("settings", Some(b"captured config".as_slice()))],
        ),
        (
            vec!["config", "config"],
            vec![("config", Some(b"captured config".as_slice()))],
        ),
    ] {
        assert!(
            authenticated_extraction_context("rust", &inventory, &observed, &captured).is_err(),
            "{inventory:?} {observed:?}"
        );
    }
    let mut forged_digest = captured.clone();
    forged_digest.config_hash = "not-a-captured-digest".into();
    assert!(
        authenticated_extraction_context(
            "rust",
            &["config"],
            &[("config", Some(b"captured config"))],
            &forged_digest,
        )
        .is_err()
    );
}

#[test]
fn failed_precondition_mints_no_occurrence() {
    let source = file("function f() { g(); if (x) { h(); } }");
    assert!(selected(&source, "revision:v2:r1", "native-v2").is_err());
    let mut forged = source.clone();
    forged.hash = "0".repeat(64);
    let mut artifact = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
    artifact.calls.clear();
    artifact.control_regions.clear();
    assert!(extract(&mut artifact, &forged, &mut IdentityRegistry::default()).is_err());
    forged = source.clone();
    forged.text.push_str(" // changed after admission");
    assert!(extract(&mut artifact, &forged, &mut IdentityRegistry::default()).is_err());
    assert!(artifact.calls.is_empty() && artifact.control_regions.is_empty());
}

#[test]
fn cached_witness_sort_matches_legacy_and_preserves_equal_key_order() {
    let values = [
        serde_json::json!({"b": 2, "a": 1}),
        serde_json::json!({"a": 0}),
        serde_json::json!({"a": 1, "b": 2}),
        serde_json::json!({"a": 1, "b": 2}),
    ];
    let mut legacy: Vec<_> = values.iter().cloned().enumerate().collect();
    let mut cached = legacy.clone();
    legacy.sort_by_key(|(_, value)| canonical(value));
    cached.sort_by_cached_key(|(_, value)| canonical(value));
    assert_eq!(cached, legacy);
    assert_eq!(
        cached
            .iter()
            .map(|(ordinal, _)| *ordinal)
            .collect::<Vec<_>>(),
        [1, 0, 2, 3]
    );
    let source = file("function f() { g(); if (x) { h(); } }");
    let a = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
    let b = selected(&source, "revision:v2:r2", NATIVE_VERSION).unwrap();
    let witness = |artifact: &Artifact| {
        document_witness(
            &artifact.revision.documents[0],
            &artifact.producer,
            &artifact.coverage[0],
            &artifact.declarations,
            &artifact.calls,
            &artifact.control_regions,
            &artifact.references,
            &artifact.capability_reports[0],
        )
        .unwrap()
    };
    let mut altered = a.clone();
    altered.references.pop();
    assert_ne!(
        witness(&a),
        witness(&altered),
        "reference inventory enters native witness"
    );
    let mut altered = a.clone();
    altered.capability_reports[0].families[0]
        .gaps
        .push(MeasurementGap {
            range: Range { start: 0, end: 1 },
            diagnostic: "measured gap".into(),
        });
    assert_ne!(
        witness(&a),
        witness(&altered),
        "capabilities enter native witness"
    );
    assert_eq!(
        witness(&a),
        witness(&b),
        "revision labels do not enter native witness"
    );
    assert!(a.calls.iter().all(|row| row.id.starts_with("occ:v2:")));
}

#[test]
fn duplicate_call_region_is_rejected_during_native_validation() {
    let source = file("function f() { if (x) { h(); } }");
    let revision = format!("revision:v2:{}", hash(b"r1"));
    let mut artifact = selected(&source, &revision, NATIVE_VERSION).unwrap();
    assert_eq!(artifact.calls.len(), 1);
    assert_eq!(artifact.calls[0].region_ids.len(), 1);
    artifact
        .validate_structure(std::slice::from_ref(&source))
        .unwrap();

    let region_id = artifact.calls[0].region_ids[0].clone();
    artifact.calls[0].region_ids.push(region_id);
    let error = artifact
        .validate_structure(std::slice::from_ref(&source))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("duplicate native call region"),
        "unexpected validation error: {error:#}"
    );
}

#[test]
fn document_version_keeps_occurrence_ids_across_revisions() {
    let source = file("function f() { g(); if (x) { h(); } }");
    let r1 = selected(&source, "revision:v2:r1", NATIVE_VERSION).unwrap();
    let r2 = selected(&source, "revision:v2:r2", NATIVE_VERSION).unwrap();
    assert_eq!(r1.calls.len(), 2);
    assert_eq!(r1.control_regions.len(), 1);
    for (a, b) in r1.calls.iter().zip(&r2.calls) {
        assert!(a.id.starts_with("occ:v2:"));
        assert_eq!(a.id, b.id);
        assert_eq!(a.region_ids, b.region_ids);
        assert_eq!(
            (a.revision_id.as_str(), b.revision_id.as_str()),
            ("revision:v2:r1", "revision:v2:r2")
        );
    }
    assert_eq!(r1.control_regions[0].id, r2.control_regions[0].id);
    assert_ne!(r1.provenance[0].id, r2.provenance[0].id);
    let edited = selected(
        &file("function f() { g(); if (x) { h(); } } "),
        "revision:v2:r2",
        NATIVE_VERSION,
    )
    .unwrap();
    assert!(
        r1.calls
            .iter()
            .zip(&edited.calls)
            .all(|(a, b)| a.id != b.id)
    );
    assert_ne!(r1.control_regions[0].id, edited.control_regions[0].id);
}
