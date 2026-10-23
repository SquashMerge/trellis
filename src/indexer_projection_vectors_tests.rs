//! Frozen SQL payload bytes written by unmodified main 0532a70. Never update
//! these vectors from `project_document` or the candidate writer.
use crate::{
    capture::Capture,
    classes::{Catalog, FileExtraction, Limits},
    indexer::{IndexOptions, NativeDocument, project_document},
    native_evidence::{Call, ControlRegion, Declaration},
    native_ids,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

// These native DTOs were decoded from main's published index.db, not extracted
// in this worktree. Source bytes and SCIP labels remain separate test inputs.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MainNativeDocument {
    document_version_id: String,
    declarations: Vec<Declaration>,
    calls: Vec<Call>,
    control_regions: Vec<ControlRegion>,
}

fn main_native(root: &Path, expected_sha: &str) -> BTreeMap<String, MainNativeDocument> {
    let bytes = fs::read(root.join("native-main.json")).unwrap();
    assert_eq!(sha(&bytes), expected_sha, "frozen main native input drift");
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        value["writerCommit"],
        "0532a7090003904317136aeac98a23716aea1fc2"
    );
    assert_eq!(
        value["writerBinarySha256"],
        "536cdd7231b66c7a8e807d528084b6f00377a96d425ca6c5183a7a7728ff7d9f"
    );
    serde_json::from_value(value["documents"].clone()).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn compare_rows<T: Serialize>(expected: &Value, rows: &[T], family: &str) {
    let frozen = expected[family].as_array().expect("main writer family");
    assert_eq!(
        rows.len(),
        frozen.len(),
        "{family} count, including empty families"
    );
    for (row, old) in rows.iter().zip(frozen) {
        let actual = serde_json::to_vec(row).unwrap();
        let bytes = old["payload"].as_str().unwrap().as_bytes();
        assert_eq!(sha(bytes), old["sha256"].as_str().unwrap());
        assert_eq!(actual.as_slice(), bytes, "{family} payload {}", old["id"]);
        let value: Value = serde_json::from_slice(&actual).unwrap();
        assert_eq!(value["id"], old["id"], "{family} row id");
    }
}

// SQL vectors are stored in ID order, which can hide writer-order regressions.
// Derive expected call order from the independently frozen main SQL payloads,
// and compare it before the byte-level checks sort projected rows by ID.
fn assert_writer_order(
    expected: &Value,
    nodes: &[crate::model::Symbol],
    calls: &[crate::model::CallSite],
    regions: &[crate::model::ControlRegion],
) {
    for (family, ids) in [
        (
            "nodes",
            nodes.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
        ),
        (
            "regions",
            regions.iter().map(|row| row.id.as_str()).collect(),
        ),
    ] {
        let frozen_ids: Vec<_> = expected[family]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids, frozen_ids,
            "project_document {family} order before sort"
        );
    }
    let mut frozen_calls: Vec<_> = expected["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| serde_json::from_str::<Value>(row["payload"].as_str().unwrap()).unwrap())
        .collect();
    frozen_calls.sort_by(|a, b| {
        (
            a["path"].as_str().unwrap(),
            a["range"]["startByte"].as_u64().unwrap(),
            a["range"]["endByte"].as_u64().unwrap(),
            a["id"].as_str().unwrap(),
        )
            .cmp(&(
                b["path"].as_str().unwrap(),
                b["range"]["startByte"].as_u64().unwrap(),
                b["range"]["endByte"].as_u64().unwrap(),
                b["id"].as_str().unwrap(),
            ))
    });
    let expected_ids: Vec<_> = frozen_calls
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let actual_ids: Vec<_> = calls.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        actual_ids, expected_ids,
        "project_document call writer order before sort"
    );
}

#[test]
fn main_writer_graph_payloads_match_one_shared_projection_byte_for_byte() {
    let root =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph-payload-main-0532a70");
    let frozen: Value =
        serde_json::from_slice(&fs::read(root.join("vectors.json")).unwrap()).unwrap();
    assert_eq!(
        frozen["writerCommit"],
        "0532a7090003904317136aeac98a23716aea1fc2"
    );
    assert_eq!(
        frozen["writerBinarySha256"],
        "536cdd7231b66c7a8e807d528084b6f00377a96d425ca6c5183a7a7728ff7d9f"
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let native = main_native(
        &root,
        "8c47ae03958e0955b1e42d5d3310d6cb38be1b72d34658509a4392cd148d762d",
    );
    for mode in ["plain", "scip"] {
        let mut options = IndexOptions::new(root.clone());
        if mode == "scip" {
            options.scip_path = Some(root.join("labels.scip"));
            options.manifest_path = Some(root.join("labels-manifest.json"));
            assert_eq!(
                sha(&fs::read(root.join("labels.scip")).unwrap()),
                frozen["modes"][mode]["scipSha256"]
            );
        }
        let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
        let documents = frozen["modes"][mode]["documents"].as_object().unwrap();
        assert_eq!(documents.len(), 5);
        assert_eq!(native.len(), documents.len());
        let mut extractions = Vec::new();
        let mut projected = BTreeMap::new();
        let mut symbol_count = 0;
        let mut payload_count = 0;
        for (path, old) in documents {
            let source = fs::read(root.join(path)).unwrap();
            assert_eq!(
                sha(&source),
                old["sourceSha256"],
                "{mode} {path} input drift"
            );
            let mut labels = BTreeMap::new();
            for node in old["rows"]["nodes"].as_array().unwrap() {
                let payload: Value =
                    serde_json::from_str(node["payload"].as_str().unwrap()).unwrap();
                if let Some(label) = payload["displayLabel"].as_str() {
                    labels.insert(node["id"].as_str().unwrap().to_owned(), label.to_owned());
                }
            }
            let facts = &native[path];
            assert_eq!(facts.document_version_id, old["documentVersionId"]);
            let mut declarations: Vec<_> = facts.declarations.iter().collect();
            // Simulates SQL attestation's syntax-id order, even when child precedes parent.
            declarations.sort_by(|a, b| a.syntax_id.cmp(&b.syntax_id));
            let mut calls: Vec<_> = facts.calls.iter().collect();
            calls.sort_by(|a, b| a.id.cmp(&b.id));
            let mut control_regions: Vec<_> = facts.control_regions.iter().collect();
            control_regions.sort_by(|a, b| a.id.cmp(&b.id));
            let (mut nodes, mut calls, mut regions) = project_document(
                path,
                &source,
                NativeDocument {
                    declarations,
                    calls,
                    control_regions,
                },
                &labels,
            )
            .unwrap();
            assert_writer_order(&old["rows"], &nodes, &calls, &regions);
            nodes.sort_by(|a, b| a.id.cmp(&b.id));
            calls.sort_by(|a, b| a.id.cmp(&b.id));
            regions.sort_by(|a, b| a.id.cmp(&b.id));
            compare_rows(&old["rows"], &nodes, "nodes");
            compare_rows(&old["rows"], &calls, "calls");
            compare_rows(&old["rows"], &regions, "regions");
            symbol_count += nodes.len();
            payload_count += nodes.len() + calls.len() + regions.len();
            if matches!(path.as_str(), "java.java" | "python.py") {
                let file = capture.files.iter().find(|f| f.path == *path).unwrap();
                let extracted =
                    FileExtraction::extract_file(file, &nodes, &cancel, Limits::default()).unwrap();
                assert_eq!(
                    serde_json::to_string(&extracted).unwrap(),
                    old["classExtraction"],
                    "{mode} {path} main class extraction F"
                );
                extractions.push(extracted);
            }
            projected.insert(path.as_str(), old);
            let graph_input = json!({
                "documentVersionId": old["documentVersionId"],
                "nodes": nodes, "calls": calls, "regions": regions,
            });
            let hash = native_ids::digest(
                b"trellis.graph-projection.v1\0",
                &native_ids::canonical(&graph_input),
            );
            assert_eq!(hash, old["graphHash"], "{mode} {path} main graph hash");
            assert_eq!(
                format!("graph:v1:{hash}"),
                old["graphProjectionId"],
                "{mode} {path} graph id"
            );
            if !matches!(path.as_str(), "java.java" | "python.py") {
                assert_eq!(old["classExtraction"], "");
            }
        }
        assert_eq!(payload_count, 56, "{mode} complete main payload corpus");
        let catalog = Catalog::compose(
            &extractions,
            capture.files.len(),
            symbol_count,
            Limits::default(),
        )
        .unwrap();
        for (path, old) in projected {
            let mut classes: Vec<_> = catalog
                .classes
                .iter()
                .filter(|class| class.symbol.path == path)
                .collect();
            classes.sort_by(|a, b| a.symbol.id.cmp(&b.symbol.id));
            let mut relations: Vec<_> = catalog
                .relations
                .iter()
                .filter(|relation| relation.path == path)
                .collect();
            relations.sort_by(|a, b| a.id.cmp(&b.id));
            let input = json!({"graphProjectionId": old["graphProjectionId"],
                "classes": classes, "relations": relations});
            let class_hash = native_ids::digest(
                b"trellis.class-projection.v1\0",
                &native_ids::canonical(&input),
            );
            assert_eq!(
                class_hash, old["classHash"],
                "{mode} {path} main class hash"
            );
            assert_eq!(
                format!("class-projection:v1:{class_hash}"),
                old["classProjectionId"],
                "{mode} {path} main class projection id"
            );
        }
    }
}

#[test]
fn main_writer_nested_call_and_query_view_oracle() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/graph-payload-main-0532a70-ties");
    let frozen: Value =
        serde_json::from_slice(&fs::read(root.join("oracle.json")).unwrap()).unwrap();
    assert_eq!(
        frozen["writerCommit"],
        "0532a7090003904317136aeac98a23716aea1fc2"
    );
    assert_eq!(
        frozen["writerBinarySha256"],
        "536cdd7231b66c7a8e807d528084b6f00377a96d425ca6c5183a7a7728ff7d9f"
    );
    let path = "nested.js";
    let source = fs::read(root.join(path)).unwrap();
    assert_eq!(sha(&source), frozen["sourceSha256"]);
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.clone()), &cancel, &|_| {}).unwrap();
    let native = main_native(
        &root,
        "b668a53cd112cee9158db05f2f8db5278924cde3a7ac1b3330d8c118684f2027",
    );
    assert_eq!(native.len(), 1);
    let facts = &native[path];
    assert_eq!(facts.document_version_id, frozen["documentVersionId"]);
    let mut declarations: Vec<_> = facts.declarations.iter().collect();
    declarations.sort_by(|a, b| a.syntax_id.cmp(&b.syntax_id));
    let mut calls: Vec<_> = facts.calls.iter().collect();
    calls.sort_by(|a, b| a.id.cmp(&b.id));
    let mut control_regions: Vec<_> = facts.control_regions.iter().collect();
    control_regions.sort_by(|a, b| a.id.cmp(&b.id));
    let (mut nodes, mut calls, mut regions) = project_document(
        path,
        &source,
        NativeDocument {
            declarations,
            calls,
            control_regions,
        },
        &BTreeMap::new(),
    )
    .unwrap();
    assert_writer_order(&frozen["rows"], &nodes, &calls, &regions);
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    calls.sort_by(|a, b| a.id.cmp(&b.id));
    regions.sort_by(|a, b| a.id.cmp(&b.id));
    compare_rows(&frozen["rows"], &nodes, "nodes");
    compare_rows(&frozen["rows"], &calls, "calls");
    compare_rows(&frozen["rows"], &regions, "regions");
    let graph_input = json!({"documentVersionId": frozen["documentVersionId"],
        "nodes": nodes, "calls": calls, "regions": regions});
    let graph_hash = native_ids::digest(
        b"trellis.graph-projection.v1\0",
        &native_ids::canonical(&graph_input),
    );
    assert_eq!(
        graph_hash, frozen["graphHash"],
        "main graph hash must not change"
    );
    assert_eq!(
        format!("graph:v1:{graph_hash}"),
        frozen["graphProjectionId"]
    );
    assert_eq!(frozen["classExtraction"], "");
    let catalog =
        Catalog::compose(&[], capture.files.len(), nodes.len(), Limits::default()).unwrap();
    let class_input = json!({"graphProjectionId": frozen["graphProjectionId"],
        "classes": catalog.classes, "relations": catalog.relations});
    let class_hash = native_ids::digest(
        b"trellis.class-projection.v1\0",
        &native_ids::canonical(&class_input),
    );
    assert_eq!(
        class_hash, frozen["classHash"],
        "main class hash must not change"
    );
    assert_eq!(
        format!("class-projection:v1:{class_hash}"),
        frozen["classProjectionId"]
    );

    // Main 0532's SQL orders endByte DESC before id. These two real source calls
    // have the same startByte but different endByte. The second row is the
    // max_calls+1 overflow probe. Excluding the seed path removes both calls.
    let ids: Vec<&str> = frozen["rows"]["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    let longer = calls
        .iter()
        .max_by_key(|call| call.range.end_byte)
        .unwrap()
        .id
        .as_str();
    let shorter = calls
        .iter()
        .min_by_key(|call| call.range.end_byte)
        .unwrap()
        .id
        .as_str();
    assert_ne!(longer, shorter);
    assert_eq!(frozen["queries"]["max1"]["calls"], json!([longer]));
    assert_eq!(frozen["queries"]["max1"]["truncated"], true);
    assert_eq!(frozen["queries"]["max2"]["calls"], json!([longer, shorter]));
    assert_eq!(frozen["queries"]["max2"]["truncated"], false);
    assert_eq!(frozen["queries"]["excluded"]["calls"], json!([]));
    assert_eq!(frozen["queries"]["excluded"]["truncated"], false);
    assert_eq!(frozen["queries"]["excluded"]["excludePaths"], json!([path]));
    assert_eq!(ids.len(), 2);
}
