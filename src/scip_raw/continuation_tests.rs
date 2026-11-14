use super::*;
use std::sync::{Arc, atomic::AtomicBool};

#[test]
fn cursor_yields_for_every_original_wire_unit_and_preserves_claims() {
    let documents = tests::vector_documents(tests::FULL);
    let wire = [
        tests::message(1, &[0x20, 1]),
        hex::decode(tests::EXTERNAL).unwrap(),
    ]
    .concat();
    let input = tests::vector_input(&wire, documents);
    let roots = ModuleRoots::default();
    let cargo = CargoTargets::default();
    let artifact = AuthenticatedArtifact {
        artifact_hash: &input.artifact_hash,
        producer_id: &input.producer_id,
        producer_version: &input.producer_version,
        source_set_id: &input.source_set_id,
        source_revision: &input.source_revision,
        position_encoding: input.position_encoding,
        protocol_version: input.protocol_version,
        text_document_encoding: input.text_document_encoding,
        project_root_uri: &input.project_root_uri,
        module_roots: &roots,
        cargo_targets: &cargo,
        assigned_documents: 2,
    };
    let flag = Arc::new(AtomicBool::new(false));
    let limits = BatchLimits::PRODUCTION;
    let mut counters = BatchCounters::default();
    let mut sink = VecSink::default();
    let mut cursor =
        ScipDecodeContinuation::new(wire.as_slice(), wire.len() as u64, &limits).unwrap();
    let mut resolve = |path: &str, _: &str| {
        let doc = input
            .documents
            .iter()
            .find(|doc| doc.relative_path == path)
            .unwrap();
        Ok(DocumentSource {
            language: doc.language.clone(),
            bytes: Cow::Borrowed(doc.source_bytes),
            content_hash: doc.content_hash.clone(),
        })
    };
    for expected_documents in [0, 1, 2, 2] {
        assert!(
            cursor
                .advance(
                    &artifact,
                    &mut resolve,
                    &mut counters,
                    &limits,
                    &mut sink,
                    &flag
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(cursor.documents, expected_documents);
        assert_eq!(counters.live_unit_buffers, 0);
    }
    let summary = cursor
        .advance(
            &artifact,
            &mut resolve,
            &mut counters,
            &limits,
            &mut sink,
            &flag,
        )
        .unwrap()
        .unwrap();
    assert_eq!(summary.documents, 2);
    assert_eq!(summary.external_symbols, 1);
    assert_eq!(
        sink.finish().unwrap(),
        decode_authenticated(&input, &flag).unwrap().claims
    );
    assert_eq!(counters.live_unit_buffers_high_water, 1);
}

#[test]
fn cursor_retains_global_wire_budget_and_cancellation_does_not_advance() {
    let wire = [tests::message(1, &[]), tests::message(1, &[])].concat();
    let roots = ModuleRoots::default();
    let cargo = CargoTargets::default();
    let artifact = AuthenticatedArtifact {
        artifact_hash: "",
        producer_id: "",
        producer_version: "",
        source_set_id: "",
        source_revision: "",
        position_encoding: 1,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "",
        module_roots: &roots,
        cargo_targets: &cargo,
        assigned_documents: 0,
    };
    let mut limits = BatchLimits::PRODUCTION;
    limits.wire_fields = 1;
    let flag = Arc::new(AtomicBool::new(true));
    let mut counters = BatchCounters::default();
    let mut sink = VecSink::default();
    let mut cursor =
        ScipDecodeContinuation::new(wire.as_slice(), wire.len() as u64, &limits).unwrap();
    let mut resolve = |_: &str, _: &str| Err(invalid("unexpected document"));
    assert_eq!(
        cursor
            .advance(
                &artifact,
                &mut resolve,
                &mut counters,
                &limits,
                &mut sink,
                &flag
            )
            .unwrap_err(),
        ScipCaptureError::Cancelled
    );
    assert_eq!(cursor.input.remaining, wire.len() as u64);
    flag.store(false, Ordering::Relaxed);
    assert!(
        cursor
            .advance(
                &artifact,
                &mut resolve,
                &mut counters,
                &limits,
                &mut sink,
                &flag
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cursor
            .advance(
                &artifact,
                &mut resolve,
                &mut counters,
                &limits,
                &mut sink,
                &flag
            )
            .unwrap_err(),
        limit("wire fields")
    );
}
