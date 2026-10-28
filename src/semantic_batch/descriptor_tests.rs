use super::{
    BatchLimits,
    descriptor::{
        ArtifactEntry, CoverageRow, Descriptor, Disposition, ExclusionReason, Page, PageEntry,
        PageKind, RowOrder, SourceManifestHasher, SourceRow, basis_artifact_ordinal,
        check_page_bytes, parse_page,
    },
};
use crate::{
    native_evidence as native,
    native_ids::{canonical, digest},
    scip_admission::pinned_producer_by_id,
    scip_raw::{self, Completion, ScipCaptureError},
};
use serde_json::{Value, json};

fn hex(c: char) -> String {
    c.to_string().repeat(64)
}

fn descriptor_json() -> Value {
    let producer = pinned_producer_by_id("trellis-synthetic-scip").unwrap();
    json!({
        "formatVersion": 1,
        "trustOrigin": "syntheticProfile",
        "profileId": "trellis-synthetic-scip-v1",
        "decoderVersion": scip_raw::DECODER_VERSION,
        "producer": {
            "name": producer.name, "version": producer.version, "identityKind": "generatorSource",
            "executableHash": producer.executable_hash, "positionEncoding": 1,
            "languages": producer.languages,
        },
        "sourceSet": {"id": "set", "rootId": "root", "languages": ["java", "javascript"], "dependencies": []},
        "sourceRevision": "revision", "sourceManifestHash": hex('a'),
        "components": {"toolchainHash": hex('b'), "configHash": hex('c'), "dependencyHash": hex('d')},
        "metadata": {"protocolVersion": 0, "projectRoot": "file:///work", "textDocumentEncoding": 1},
        "completion": "complete",
        "totals": {"sourceDocuments": 3, "coveredDocuments": 3, "excludedDocuments": 0, "pageBytes": 300, "artifactBytes": 30},
        "pages": [
            {"kind": "source", "ordinal": 0, "relativePath": "p/s0.json", "byteLength": 100, "sha256": hex('1'), "rowCount": 2},
            {"kind": "source", "ordinal": 1, "relativePath": "p/s1.json", "byteLength": 100, "sha256": hex('2'), "rowCount": 1},
            {"kind": "coverage", "ordinal": 0, "relativePath": "p/c0.json", "byteLength": 100, "sha256": hex('3'), "rowCount": 3},
        ],
        "artifacts": [
            {"ordinal": 0, "relativePath": "a/0.scip", "byteLength": 10, "sha256": hex('4'), "language": "java"},
            {"ordinal": 1, "relativePath": "a/1.scip", "byteLength": 20, "sha256": hex('5'), "language": "javascript"},
        ],
    })
}

fn descriptor_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

type Change = fn(&mut Descriptor);

fn limits() -> BatchLimits {
    BatchLimits::PRODUCTION
}

fn admission_detail(error: ScipCaptureError) -> String {
    match error {
        ScipCaptureError::InvalidAdmission(detail) => detail,
        other => panic!("expected an admission error, got {other:?}"),
    }
}

#[test]
fn incremental_source_manifest_hash_equals_canonical_array_hash() {
    let rows = [
        ("java", "java/A.java", hex('1')),
        ("rust", "rust/lib.rs", hex('2')),
        ("javascript", "js/a.js", hex('3')),
    ];
    let documents: Vec<_> = rows
        .iter()
        .map(|(language, path, hash)| native::Document {
            key: native::DocumentKey {
                source_set_id: "set".into(),
                language: (*language).into(),
                path: (*path).into(),
            },
            revision_id: "revision".into(),
            content_hash: hash.clone(),
            byte_length: 1,
        })
        .collect();
    let revision = native::Revision {
        id: "revision".into(),
        source_set_id: "set".into(),
        documents,
        toolchain_hash: hex('b'),
        config_hash: hex('c'),
        dependency_hash: hex('d'),
    };
    let mut hasher = SourceManifestHasher::new("set");
    for (language, path, hash) in &rows {
        hasher.push(language, path, hash);
    }
    let incremental = hasher.finish();
    let array: Vec<Value> = rows
        .iter()
        .map(|(language, path, hash)| {
            json!({"document": {"sourceSetId": "set", "language": language, "path": path}, "contentHash": hash})
        })
        .collect();
    assert_eq!(incremental, digest(b"", &canonical(&json!(array))));
    assert_eq!(incremental, scip_raw::sha(&canonical(&json!(array))));
    assert_eq!(
        incremental,
        crate::semantic_import::manifest_hash(&revision)
    );
    assert_eq!(
        SourceManifestHasher::new("set").finish(),
        scip_raw::sha(b"[]")
    );
}

#[test]
fn descriptor_grammar_rejects_unknown_keys_and_versions() {
    let valid = descriptor_json();
    let bytes = descriptor_bytes(&valid);
    let parsed = Descriptor::parse_admitted(&bytes, &scip_raw::sha(&bytes), &limits()).unwrap();
    assert_eq!(parsed.completion, Completion::Complete);
    assert_eq!(parsed, Descriptor::parse(&bytes).unwrap());
    assert_eq!(
        serde_json::from_slice::<Value>(&serde_json::to_vec(&parsed).unwrap()).unwrap(),
        valid
    );

    for pointer in [
        "",
        "/producer",
        "/sourceSet",
        "/components",
        "/metadata",
        "/totals",
        "/pages/0",
        "/artifacts/0",
    ] {
        let mut changed = valid.clone();
        let target = if pointer.is_empty() {
            &mut changed
        } else {
            changed.pointer_mut(pointer).unwrap()
        };
        target["extra"] = json!(1);
        assert!(
            Descriptor::parse(&descriptor_bytes(&changed)).is_err(),
            "unknown key at {pointer:?} was accepted"
        );
    }
    for key in ["completion", "totals", "pages", "artifacts", "metadata"] {
        let mut changed = valid.clone();
        changed.as_object_mut().unwrap().remove(key);
        assert!(Descriptor::parse(&descriptor_bytes(&changed)).is_err());
    }
    let mut removed = valid.clone();
    removed["sourceSet"]
        .as_object_mut()
        .unwrap()
        .remove("dependencies");
    assert!(Descriptor::parse(&descriptor_bytes(&removed)).is_err());
    let mut kind = valid.clone();
    kind["producer"]["identityKind"] = json!("signed");
    assert!(Descriptor::parse(&descriptor_bytes(&kind)).is_err());
    let mut completion = valid.clone();
    completion["completion"] = json!("mostly");
    assert!(Descriptor::parse(&descriptor_bytes(&completion)).is_err());

    let mut version = valid.clone();
    version["formatVersion"] = json!(2);
    let bytes = descriptor_bytes(&version);
    assert_eq!(
        admission_detail(
            Descriptor::parse_admitted(&bytes, &scip_raw::sha(&bytes), &limits()).unwrap_err()
        ),
        "descriptor format version"
    );
    let mut decoder = valid.clone();
    decoder["decoderVersion"] = json!("scip-0.10.0/CapturedScipFactV1/1");
    let bytes = descriptor_bytes(&decoder);
    assert_eq!(
        admission_detail(
            Descriptor::parse_admitted(&bytes, &scip_raw::sha(&bytes), &limits()).unwrap_err()
        ),
        "descriptor decoder version"
    );
    let bytes = descriptor_bytes(&valid);
    assert_eq!(
        admission_detail(Descriptor::parse_admitted(&bytes, &hex('0'), &limits()).unwrap_err()),
        "descriptor digest"
    );
    let oversized = vec![b' '; 1024 * 1024 + 1];
    assert!(matches!(
        Descriptor::parse_admitted(&oversized, &scip_raw::sha(&oversized), &limits()),
        Err(ScipCaptureError::LimitExceeded(name)) if name == "descriptor bytes"
    ));
}

#[test]
fn descriptor_checks_pin_the_producer_and_total_the_entries() {
    let descriptor = Descriptor::parse(&descriptor_bytes(&descriptor_json())).unwrap();
    assert_eq!(
        descriptor.check_producer().unwrap().trust_origin,
        "syntheticProfile"
    );
    descriptor.check_pages(&limits()).unwrap();
    descriptor.check_artifacts(&limits()).unwrap();

    let mut crossed = descriptor.clone();
    crossed.trust_origin = "referenceProfile".into();
    assert!(crossed.check_producer().is_err());
    let mut hash = descriptor.clone();
    hash.producer.executable_hash = hex('0');
    assert_eq!(
        admission_detail(hash.check_producer().unwrap_err()),
        "producer profile pin"
    );

    let page_cases: [(&str, Change); 5] = [
        ("page totals", |d| d.totals.page_bytes += 1),
        ("page totals", |d| d.totals.source_documents += 1),
        ("page ordinals", |d| d.pages[1].ordinal = 5),
        ("page order", |d| d.pages.swap(1, 2)),
        ("coverage completion", |d| {
            d.completion = Completion::Partial
        }),
    ];
    for (detail, change) in page_cases {
        let mut changed = descriptor.clone();
        change(&mut changed);
        assert_eq!(
            admission_detail(changed.check_pages(&limits()).unwrap_err()),
            detail
        );
    }
    let mut too_many = descriptor.clone();
    too_many.pages[0].row_count = 1_001;
    assert!(matches!(
        too_many.check_pages(&limits()),
        Err(ScipCaptureError::LimitExceeded(name)) if name == "page rows"
    ));

    let artifact_cases: [(&str, Change); 4] = [
        ("artifact language", |d| {
            d.artifacts[0].language = "go".into()
        }),
        ("artifact totals", |d| d.totals.artifact_bytes = 1),
        ("duplicate artifact", |d| {
            d.artifacts[1].sha256 = d.artifacts[0].sha256.clone()
        }),
        ("artifact ordinals", |d| d.artifacts[1].ordinal = 3),
    ];
    for (detail, change) in artifact_cases {
        let mut changed = descriptor.clone();
        change(&mut changed);
        assert_eq!(
            admission_detail(changed.check_artifacts(&limits()).unwrap_err()),
            detail
        );
    }
    let mut none = descriptor.clone();
    none.artifacts.clear();
    assert_eq!(
        admission_detail(none.check_artifacts(&limits()).unwrap_err()),
        "artifact count"
    );
}

fn page_entry(bytes: &[u8]) -> PageEntry {
    PageEntry {
        kind: PageKind::Source,
        ordinal: 0,
        relative_path: "p.json".into(),
        byte_length: bytes.len() as u64,
        sha256: scip_raw::sha(bytes),
        row_count: 1,
    }
}

#[test]
fn page_grammar_rejects_unknown_keys_and_row_shapes() {
    let valid = json!({
        "formatVersion": 1, "kind": "source", "ordinal": 0,
        "rows": [{"byteLength": 12, "contentHash": hex('a'), "language": "java", "relativePath": "java/A.java"}],
    });
    let bytes = serde_json::to_vec(&valid).unwrap();
    let page: Page<SourceRow> = parse_page(&bytes).unwrap();
    let entry = page_entry(&bytes);
    check_page_bytes(&entry, &bytes).unwrap();
    page.check_entry(&entry).unwrap();
    page.rows[0].check(&["java".into()]).unwrap();
    assert!(page.rows[0].check(&["rust".into()]).is_err());

    let mut unknown = valid.clone();
    unknown["extra"] = json!(true);
    assert!(parse_page::<SourceRow>(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let mut row_key = valid.clone();
    row_key["rows"][0]["extra"] = json!(true);
    assert!(parse_page::<SourceRow>(&serde_json::to_vec(&row_key).unwrap()).is_err());
    let mut missing = valid.clone();
    missing["rows"][0]
        .as_object_mut()
        .unwrap()
        .remove("byteLength");
    assert!(parse_page::<SourceRow>(&serde_json::to_vec(&missing).unwrap()).is_err());
    let mut kind = valid.clone();
    kind["kind"] = json!("symbols");
    assert!(parse_page::<SourceRow>(&serde_json::to_vec(&kind).unwrap()).is_err());

    let mut reordered = page.clone();
    reordered.ordinal = 1;
    assert_eq!(
        admission_detail(reordered.check_entry(&entry).unwrap_err()),
        "reordered page"
    );
    let mut counted = entry.clone();
    counted.row_count = 2;
    assert_eq!(
        admission_detail(page.check_entry(&counted).unwrap_err()),
        "page rows"
    );
    let mut short = entry.clone();
    short.byte_length += 1;
    assert_eq!(
        admission_detail(check_page_bytes(&short, &bytes).unwrap_err()),
        "page length"
    );
    let mut digest = entry.clone();
    digest.sha256 = hex('0');
    assert_eq!(
        admission_detail(check_page_bytes(&digest, &bytes).unwrap_err()),
        "page digest"
    );
}

#[test]
fn rows_must_strictly_increase_across_pages() {
    let mut source = RowOrder::source();
    source.push("java", "a").unwrap();
    source.push("java", "b").unwrap();
    source.push("rust", "a").unwrap();
    assert_eq!(
        admission_detail(source.push("rust", "a").unwrap_err()),
        "duplicate source path"
    );
    assert_eq!(
        admission_detail(source.push("java", "z").unwrap_err()),
        "unsorted source rows"
    );
    assert!(RowOrder::coverage().push("cobol", "a").is_err());
    let mut coverage = RowOrder::coverage();
    coverage.push("python", "a").unwrap();
    assert_eq!(
        admission_detail(coverage.push("python", "a").unwrap_err()),
        "duplicate coverage row"
    );
}

#[test]
fn coverage_row_requires_exactly_one_disposition() {
    let row = |artifact: Value, exclusion: Value| {
        serde_json::from_value::<CoverageRow>(json!({
            "artifact": artifact, "exclusion": exclusion, "language": "java", "relativePath": "a",
        }))
        .unwrap()
    };
    assert_eq!(
        row(json!(1), Value::Null).disposition().unwrap(),
        Disposition::Covered { artifact: 1 }
    );
    assert_eq!(
        row(Value::Null, json!("unsupported"))
            .disposition()
            .unwrap(),
        Disposition::Excluded(ExclusionReason::Unsupported)
    );
    for (artifact, exclusion) in [(json!(0), json!("failed")), (Value::Null, Value::Null)] {
        assert_eq!(
            admission_detail(row(artifact, exclusion).disposition().unwrap_err()),
            "coverage disposition"
        );
    }
    assert!(
        serde_json::from_value::<CoverageRow>(
            json!({"artifact": 0, "language": "java", "relativePath": "a"})
        )
        .is_err(),
        "a null disposition key may not be omitted"
    );
    assert!(
        serde_json::from_value::<CoverageRow>(
            json!({"artifact": null, "exclusion": "skipped", "language": "java", "relativePath": "a"})
        )
        .is_err()
    );
}

fn artifact(ordinal: u32, language: &str) -> ArtifactEntry {
    ArtifactEntry {
        ordinal,
        relative_path: format!("a/{ordinal}.scip"),
        byte_length: 1,
        sha256: hex('f'),
        language: language.into(),
    }
}

#[test]
fn basis_artifact_rule_assigns_covered_and_excluded_documents() {
    let artifacts = [
        artifact(0, "java"),
        artifact(1, "javascript"),
        artifact(2, "javascript"),
    ];
    assert_eq!(basis_artifact_ordinal(&artifacts, "javascript", Some(2)), 2);
    assert_eq!(basis_artifact_ordinal(&artifacts, "java", Some(0)), 0);
    assert_eq!(basis_artifact_ordinal(&artifacts, "javascript", None), 1);
    assert_eq!(basis_artifact_ordinal(&artifacts, "rust", None), 0);
}
