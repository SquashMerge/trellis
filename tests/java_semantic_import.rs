use serde_json::json;
use trellis::{
    native_evidence as n,
    scip_admission::{CapturedClaim, CapturedScipFactV1},
    semantic_import::occurrence_range,
};

#[test]
fn java_converted_type_use_must_obey_exact_admitted_encoding() {
    let source = b"class Base {} class Child extends Base {}";
    let base = source.windows(4).rposition(|s| s == b"Base").unwrap();
    let claim = CapturedClaim {
        envelope: CapturedScipFactV1 {
            format_version: 1,
            record_kind: "occurrence".into(),
            relative_path: "Child.java".into(),
            position_encoding: 2,
            raw: json!({"range":{"encoding":"legacy","values":[0,base,base+4]},"symbol":"pkg/Base#","symbolRoles":0,"syntaxKind":0}),
        },
        canonical_bytes: vec![],
        fact_digest: "raw".into(),
        captured_proof_id: "raw".into(),
        language: "java".into(),
        artifact_hash: "0".repeat(64),
    };
    assert_eq!(
        occurrence_range(&claim, source, 2).unwrap(),
        n::Range {
            start: base,
            end: base + 4
        }
    );
    assert!(occurrence_range(&claim, source, 1).is_err());
}
