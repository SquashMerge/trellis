use super::*;
use crate::{native_evidence::DocumentKey, semantic_batch::descriptor::Descriptor};
use serde_json::json;

fn batch() -> SelectedRawBatch {
    let pinned = crate::scip_admission::pinned_producer_by_id("trellis-synthetic-scip").unwrap();
    let hex = |c: char| c.to_string().repeat(64);
    let artifact = |ordinal: u32, language: &str| {
        json!({"ordinal": ordinal, "relativePath": format!("a{ordinal}.scip"), "byteLength": 1,
               "sha256": hex(char::from(b'0' + ordinal as u8)), "language": language})
    };
    let descriptor = json!({
        "formatVersion": 1, "trustOrigin": "syntheticProfile", "profileId": pinned.profile_id,
        "decoderVersion": crate::scip_raw::DECODER_VERSION,
        "producer": {"name": pinned.name, "version": pinned.version, "identityKind": "generatorSource",
                     "executableHash": pinned.executable_hash, "positionEncoding": pinned.position_encoding,
                     "languages": pinned.languages},
        "sourceSet": {"id": "set", "rootId": "root", "languages": ["java", "javascript"], "dependencies": []},
        "sourceRevision": "revision", "sourceManifestHash": hex('a'),
        "components": {"toolchainHash": hex('b'), "configHash": hex('c'), "dependencyHash": hex('d')},
        "metadata": {"protocolVersion": 0, "projectRoot": "file:///work", "textDocumentEncoding": 1},
        "completion": "complete",
        "totals": {"sourceDocuments": 3, "coveredDocuments": 1, "excludedDocuments": 0, "pageBytes": 0, "artifactBytes": 3},
        "pages": [],
        "artifacts": [artifact(0, "java"), artifact(1, "javascript"), artifact(2, "javascript")],
    });
    let descriptor: Descriptor = serde_json::from_value(descriptor).unwrap();
    let captures = descriptor
        .artifacts
        .iter()
        .map(|entry| SelectedRawCapture {
            capture_id: 10 + i64::from(entry.ordinal),
            ordinal: entry.ordinal,
            language: entry.language.clone(),
            artifact_hash: entry.sha256.clone(),
            binary_bytes: vec![1],
        })
        .collect();
    SelectedRawBatch {
        producer: pinned.producer(),
        requested_documents: vec![],
        producer_id: pinned.name,
        source_set_id: "set".into(),
        captured_revision: "revision".into(),
        batch_id: 1,
        descriptor,
        source_manifest_hash: hex('a'),
        decoder_version: crate::scip_raw::DECODER_VERSION.into(),
        captures,
        exclusions: vec![],
        proofs: vec![],
        documents: BTreeMap::from([(
            ("javascript".into(), "b.js".into()),
            (hex('e'), hex('f'), 12),
        )]),
        captured_pin: IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: 1,
        },
    }
}

fn key(language: &str, path: &str) -> DocumentKey {
    DocumentKey {
        source_set_id: "set".into(),
        language: language.into(),
        path: path.into(),
    }
}

#[test]
fn basis_artifact_is_the_assigned_capture_else_the_lowest_ordinal_of_the_language() {
    let batch = batch();
    let hash = |ordinal: usize| batch.captures[ordinal].artifact_hash.as_str();
    assert_eq!(batch.artifact_hash_for(&key("javascript", "b.js")), hash(2));
    assert_eq!(
        batch.artifact_hash_for(&key("javascript", "excluded.js")),
        hash(1)
    );
    assert_eq!(batch.artifact_hash_for(&key("java", "A.java")), hash(0));
    assert_eq!(batch.artifact_hash_for(&key("python", "a.py")), hash(0));
}
