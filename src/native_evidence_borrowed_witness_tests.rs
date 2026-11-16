use super::*;

#[derive(Clone, Deserialize)]
struct Fixture {
    document: Document,
    producer: Producer,
    coverage: Coverage,
    declarations: Vec<Declaration>,
    calls: Vec<Call>,
    regions: Vec<ControlRegion>,
    references: Vec<Reference>,
    capability: CapabilityReport,
}

fn fixture() -> Fixture {
    let text = "quoted\"\\\n\t\r\0é中";
    let document = json!({"sourceSetId": "source-set", "language": "javascript", "path": text});
    let range = json!({"start": 1, "end": 9});
    let key = json!({"kind": "function", "name": text, "signature": {
        "parameterTypes": [text], "typeParameterCount": 1, "variadic": true
    }, "ordinal": 0});
    let declaration = json!({
        "role": "production", "syntaxId": "sid", "document": document,
        "revisionId": "revision", "kind": "function", "name": text,
        "lookupKey": "lookup", "ancestors": [key], "key": key, "range": range,
        "nameRange": range, "header": {"kind": "function", "name": text,
            "modifiers": ["static"], "typeParameters": [text], "parameters": [
                {"name": null, "type": text, "variadic": true},
                {"name": text, "type": null, "variadic": false}
            ], "resultType": null, "bases": [text]}, "provenanceId": "proof"
    });
    let mut anonymous = declaration.clone();
    anonymous["syntaxId"] = json!("anonymous");
    anonymous["name"] = Value::Null;
    anonymous["lookupKey"] = Value::Null;
    anonymous["nameRange"] = Value::Null;
    anonymous["key"]["name"] = Value::Null;
    anonymous["key"]["signature"] = Value::Null;
    anonymous["header"]["name"] = Value::Null;
    anonymous["header"]["resultType"] = json!(text);
    let call = json!({"id": "call", "ownerSyntaxId": "sid", "ordinal": 0,
        "document": document, "revisionId": "revision", "range": range,
        "calleeRange": range, "spelling": text, "regionIds": ["outer", "inner"],
        "provenanceId": "proof"});
    let mut dynamic = call.clone();
    dynamic["id"] = json!("dynamic");
    dynamic["calleeRange"] = Value::Null;
    dynamic["spelling"] = Value::Null;
    let region = json!({"id": "outer", "ownerSyntaxId": "sid", "ordinal": 0,
        "document": document, "revisionId": "revision", "kind": "if", "range": range,
        "parentId": null, "arm": null, "provenanceId": "proof"});
    let mut inner = region.clone();
    inner["id"] = json!("inner");
    inner["parentId"] = json!("outer");
    inner["arm"] = json!(text);
    let reference = json!({"id": "reference", "ownerSyntaxId": "sid", "ordinal": 0,
        "document": document, "revisionId": "revision", "range": range,
        "spelling": text, "lookupKey": text, "provenanceId": "proof"});
    let mut normalized = reference.clone();
    normalized["id"] = json!("normalized");
    normalized["lookupKey"] = json!("different");
    serde_json::from_value(json!({
        "document": {"key": document, "revisionId": "revision", "contentHash": "a".repeat(64), "byteLength": 20},
        "producer": {"id": PRODUCER, "version": NATIVE_VERSION, "executableHash": "e".repeat(64),
            "kind": "native", "languages": ["javascript"], "positionEncoding": "utf8"},
        "coverage": {"producerId": PRODUCER, "language": "javascript", "sourceSetId": "source-set",
            "documentPath": text, "revisionId": "revision", "requested": true, "selected": true,
            "state": "partial", "supportedRoles": ["production", "test"],
            "observedRoles": ["production"], "diagnostic": text},
        "declarations": [declaration, declaration, anonymous], "calls": [call, call, dynamic],
        "regions": [region, region, inner], "references": [reference, reference, normalized],
        "capability": {"document": document, "revisionId": "revision", "families": [
            {"kind": "declarationName", "available": true, "diagnostic": null, "gaps": []},
            {"kind": "callee", "available": true, "diagnostic": text,
                "gaps": [{"range": range, "diagnostic": text}]},
            {"kind": "invocation", "available": false, "diagnostic": text, "gaps": []},
            {"kind": "reference", "available": true, "diagnostic": null, "gaps": []}
        ]}
    })).unwrap()
}

// Original owned-slice normalizer, independent of the borrowed implementation.
fn legacy_rows<T: Serialize>(items: &[T]) -> Result<Vec<Value>> {
    let mut facts: Vec<Value> = items
        .iter()
        .map(|item| {
            let mut value = serde_json::to_value(item)?;
            if let Some(object) = value.as_object_mut() {
                object.remove("revisionId");
                object.remove("provenanceId");
            }
            Ok(value)
        })
        .collect::<Result<_>>()?;
    facts.sort_by_cached_key(canonical);
    Ok(facts)
}

fn family_parity<T: Serialize>(rows: &[T]) -> Vec<Value> {
    let legacy = legacy_rows(rows).unwrap();
    let owned = stable_witness_rows(rows).unwrap();
    let grouped: Vec<_> = rows.iter().collect();
    let borrowed = stable_witness_rows(grouped.iter().copied()).unwrap();
    assert_eq!(legacy, owned);
    assert_eq!(legacy, borrowed);
    assert_eq!(canonical(&json!(legacy)), canonical(&json!(owned)));
    assert_eq!(canonical(&json!(legacy)), canonical(&json!(borrowed)));
    legacy
}

// The original witness envelope and checksum path; candidate rows have separately
// passed the actual production normalizer's Value and canonical-byte parity checks.
fn legacy_bytes(f: &Fixture) -> Vec<u8> {
    let declarations = family_parity(&f.declarations);
    let calls = family_parity(&f.calls);
    let regions = family_parity(&f.regions);
    let references = family_parity(&f.references);
    let capability = family_parity(std::slice::from_ref(&f.capability));
    let mut coverage = serde_json::to_value(&f.coverage).unwrap();
    coverage.as_object_mut().unwrap().remove("revisionId");
    canonical(&json!({
        "documentKey": f.document.key, "contentHash": f.document.content_hash,
        "byteLength": f.document.byte_length, "producerId": f.producer.id,
        "producerVersion": f.producer.version,
        "extractionContext": extraction_context(&f.document.key.language, &[]).unwrap(),
        "coverage": coverage, "declarations": declarations,
        "calls": calls, "regions": regions,
        "references": references, "capabilityReports": capability,
    }))
}

fn witness(f: &Fixture) -> Result<String> {
    document_witness(
        &f.document,
        &f.producer,
        &f.coverage,
        &f.declarations,
        &f.calls,
        &f.regions,
        &f.references,
        &f.capability,
    )
}

#[test]
fn borrowed_witness_matches_original_bytes_and_checksum() {
    let f = fixture();
    let bytes = legacy_bytes(&f);
    let expected = digest(b"trellis.native-version-witness.v1\0", &bytes);
    assert_eq!(
        expected,
        "a4f7f63d363a2d45699ce8c8e85e12248c7d878551769bf64f2e0885e78c973c"
    );
    assert_eq!(witness(&f).unwrap(), expected);
    let declarations: Vec<_> = f.declarations.iter().collect();
    let calls: Vec<_> = f.calls.iter().collect();
    let regions: Vec<_> = f.regions.iter().collect();
    let references: Vec<_> = f.references.iter().collect();
    assert_eq!(
        document_witness(
            &f.document,
            &f.producer,
            &f.coverage,
            declarations.iter().copied(),
            calls.iter().copied(),
            regions.iter().copied(),
            references.iter().copied(),
            &f.capability,
        )
        .unwrap(),
        expected
    );
    let mut reordered = f.clone();
    reordered.declarations.reverse();
    reordered.calls.reverse();
    reordered.regions.reverse();
    reordered.references.reverse();
    assert_eq!(legacy_bytes(&reordered), bytes);
    assert_eq!(witness(&reordered).unwrap(), expected);
    let mut empty = f;
    empty.declarations.clear();
    empty.calls.clear();
    empty.regions.clear();
    empty.references.clear();
    assert_eq!(
        witness(&empty).unwrap(),
        digest(
            b"trellis.native-version-witness.v1\0",
            &legacy_bytes(&empty)
        )
    );
}

#[test]
fn borrowed_witness_omits_proof_labels_but_keeps_measured_fields() {
    let f = fixture();
    let expected = witness(&f).unwrap();
    let mut relabeled = f.clone();
    relabeled.document.revision_id = "another revision".into();
    relabeled.coverage.revision_id = "another revision".into();
    relabeled.capability.revision_id = "another revision".into();
    macro_rules! relabel {
        ($rows:expr) => {
            for row in &mut $rows {
                row.revision_id = "another revision".into();
                row.provenance_id = "another proof".into();
            }
        };
    }
    relabel!(relabeled.declarations);
    relabel!(relabeled.calls);
    relabel!(relabeled.regions);
    relabel!(relabeled.references);
    assert_eq!(legacy_bytes(&f), legacy_bytes(&relabeled));
    assert_eq!(witness(&relabeled).unwrap(), expected);
    let mutations: [fn(&mut Fixture); 6] = [
        |f| f.declarations[0].role = DeclarationRole::Test,
        |f| f.calls[0].spelling = Some("changed".into()),
        |f| f.regions[0].arm = Some("changed".into()),
        |f| f.references[0].lookup_key = "changed".into(),
        |f| f.coverage.diagnostic = None,
        |f| f.capability.families[0].available = false,
    ];
    for mutate in mutations {
        let mut changed = f.clone();
        mutate(&mut changed);
        assert_ne!(witness(&changed).unwrap(), expected);
    }
    let mut invalid = f;
    invalid.document.key.language = "invalid".into();
    assert_eq!(
        witness(&invalid).unwrap_err().to_string(),
        "invalid extraction-context language"
    );
}
