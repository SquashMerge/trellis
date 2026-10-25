use serde_json::json;
use trellis::native_ids::{IdentityRegistry, canonical};

#[test]
fn identity_excludes_body_and_revision_and_occurrence_is_document_version_local() {
    let descriptor = json!({"sourceSet":"core", "path":"src/A.java", "language":"java", "ancestors":[],
        "declaration":{"kind":"method", "name":"run", "signature":{"parameterTypes":[],"typeParameterCount":0,"variadic":false},"ordinal":0}});
    let mut registry = IdentityRegistry::default();
    let syntax = registry.stable(&descriptor).unwrap();
    assert_eq!(syntax, "sid:v1:a786650724b9d739d4b37f52082b66c4");
    let context = trellis::native_ids::extraction_context("java", &[]).unwrap();
    let occurrence = |content: &str, context: &str, version: &str, kind: &str| {
        json!({"contentHash":content,"extractionContext":context,"nativeProducerId":"trellis.native.syntax",
            "nativeProducerVersion":version,"ownerSyntaxId":syntax,"kind":kind,"ordinal":0})
    };
    let bytes = "a".repeat(64);
    let first = registry
        .occurrence(&occurrence(&bytes, &context, "native-v3", "call"))
        .unwrap();
    assert!(first.starts_with("occ:v2:") && first.len() == 39);
    // The containing revision is not an input: an identical document version, producer and
    // context keep one ID in every revision.
    assert_eq!(
        first,
        registry
            .occurrence(&occurrence(&bytes, &context, "native-v3", "call"))
            .unwrap()
    );
    for changed in [
        occurrence(&"b".repeat(64), &context, "native-v3", "call"),
        occurrence(&bytes, &"c".repeat(64), "native-v3", "call"),
        occurrence(&bytes, &context, "native-v4", "call"),
        occurrence(&bytes, &context, "native-v3", "control"),
    ] {
        assert_ne!(first, registry.occurrence(&changed).unwrap());
    }
    // The withdrawn revision-bound v1 input is rejected, never minted.
    for withdrawn in [
        json!({"revisionId":"r1","ownerSyntaxId":syntax,"kind":"call","ordinal":0}),
        {
            let mut v = occurrence(&bytes, &context, "native-v3", "call");
            v["revisionId"] = json!("r1");
            v
        },
    ] {
        assert!(registry.occurrence(&withdrawn).is_err());
    }
    let changed = json!({"sourceSet":"core", "path":"src/A.java", "language":"java", "ancestors":[],
        "declaration":{"kind":"method", "name":"changed", "signature":{"parameterTypes":[],"typeParameterCount":0,"variadic":false},"ordinal":0}});
    assert_ne!(syntax, registry.stable(&changed).unwrap());
}

#[test]
fn canonical_controls_are_six_ascii_bytes_and_unicode_is_not_normalized() {
    let bytes = canonical(&json!({"z":"e\u{301}","a":"é\n\\\"\u{2028}"}));
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        "{\"a\":\"é\\u000a\\\\\\\"\u{2028}\",\"z\":\"e\u{301}\"}"
    );
    assert_ne!(canonical(&json!("é")), canonical(&json!("e\u{301}")));
}

#[test]
fn four_languages_and_empty_workspace_share_closed_native_memory() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (_, empty) = index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &identity.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(empty.revision.documents.len(), 0);
    assert!(empty.coverage.is_empty() && empty.provenance.is_empty());
    for (path, source) in [
        ("A.java", "class A { void go() { obj.foo(); } }"),
        ("a.js", "function go() { obj.foo(); }"),
        ("a.py", "def go():\n    obj.foo()\n"),
        ("a.rs", "fn go() { obj.foo(); }"),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let (graph, native) = index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &identity.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(graph.files.len(), 4);
    assert_eq!(native.coverage.len(), 4);
    assert_eq!(native.provenance.len(), 4);
    for lang in ["java", "javascript", "python", "rust"] {
        let call = native
            .calls
            .iter()
            .find(|c| c.document.language == lang && c.spelling.as_deref() == Some("foo"))
            .unwrap_or_else(|| panic!("no measured foo for {lang}: {:?}", native.calls));
        let file = graph
            .files
            .iter()
            .find(|f| f.path == call.document.path)
            .unwrap();
        let callee = call.callee_range.as_ref().unwrap();
        assert_eq!(&file.text[callee.start..callee.end], "foo");
    }
}

#[test]
fn unicode_lookup_normalizes_without_changing_exact_identity_bytes() {
    use trellis::native_evidence::lookup;
    assert_eq!(lookup("rust", "e\u{301}").unwrap(), "é");
    assert_eq!(lookup("python", "Ａ").unwrap(), "A");
    assert_eq!(lookup("java", "e\u{301}").unwrap(), "e\u{301}");
    assert_eq!(lookup("javascript", "Ａ").unwrap(), "Ａ");
    // NFC and NFD spellings share one lookup key in Python and Rust only.
    for language in ["python", "rust"] {
        assert_eq!(
            lookup(language, "e\u{301}").unwrap(),
            lookup(language, "\u{e9}").unwrap(),
            "{language} NFD/NFC"
        );
    }
    for language in ["java", "javascript"] {
        assert_ne!(
            lookup(language, "e\u{301}").unwrap(),
            lookup(language, "\u{e9}").unwrap(),
            "{language} keeps NFC/NFD distinct"
        );
    }
}

#[test]
fn closed_records_reject_unknown_fields_and_fabricated_semantics() {
    use serde_json::json;
    use trellis::native_evidence::{Call, Coverage, Provenance};
    assert!(serde_json::from_value::<Call>(json!({"id":"occ:v2:dead","ownerSyntaxId":"sid:v1:dead","ordinal":0,"document":{"sourceSetId":"s","language":"rust","path":"a.rs"},"revisionId":"r","range":{"start":0,"end":1},"calleeRange":null,"spelling":null,"regionIds":[],"provenanceId":"p","target":"guessed"})).is_err());
    assert!(serde_json::from_value::<Provenance>(json!({"id":"p","producerId":"n","document":{"sourceSetId":"s","language":"rust","path":"a.rs"},"revisionId":"r","contentHash":"h","evidenceKind":"measuredSyntax","basis":null,"freshness":"fresh","derivedFrom":null,"receiver":"guessed"})).is_err());
    assert!(serde_json::from_value::<Coverage>(json!({"producerId":"n","language":"rust","sourceSetId":"s","documentPath":"a.rs","revisionId":"r","requested":true,"selected":true,"state":"complete","supportedRoles":["definition","call"],"observedRoles":[],"diagnostic":null,"reference":"inferred"})).is_err());
}

#[test]
fn source_body_only_changes_revision_not_stable_declaration() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    let source = root.path().join("a.py");
    fs::write(&source, "def greet():\n    return 1\n").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let (_, before) = index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &id.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    fs::write(&source, "def greet():\n    return 2\n").unwrap();
    let (_, after) = index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &id.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    let declaration = |a: &trellis::native_evidence::Artifact| {
        a.declarations
            .iter()
            .find(|d| d.name.as_deref() == Some("greet"))
            .unwrap()
            .syntax_id
            .clone()
    };
    assert_eq!(declaration(&before), declaration(&after));
    assert_ne!(before.revision.id, after.revision.id);
    assert_ne!(
        before.revision.documents[0].content_hash,
        after.revision.documents[0].content_hash
    );
}

#[test]
fn editing_one_document_reidentifies_only_its_own_occurrences() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    fs::write(
        root.path().join("a.js"),
        "function f() { g(); if (x) { h(); } }\n",
    )
    .unwrap();
    fs::write(root.path().join("b.js"), "function g() { k(); }\n").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let index = || {
        index_workspace_with_native(
            &IndexOptions::new(root.path().into()),
            &id.record_id,
            &cancel,
            |_| {},
        )
        .unwrap()
        .1
    };
    let before = index();
    fs::write(root.path().join("b.js"), "function g() { k(); k(); }\n").unwrap();
    let after = index();
    assert_ne!(before.revision.id, after.revision.id);
    let ids = |a: &trellis::native_evidence::Artifact, path: &str| {
        let calls = a.calls.iter().filter(|c| c.document.path == path);
        let regions = a.control_regions.iter().filter(|r| r.document.path == path);
        calls
            .map(|c| c.id.clone())
            .chain(regions.map(|r| r.id.clone()))
            .collect::<Vec<_>>()
    };
    // a.js is byte-identical under the same producer and context: same occ:v2 IDs, now
    // carried by records of the new containing revision.
    assert_eq!(ids(&before, "a.js").len(), 3);
    assert_eq!(ids(&before, "a.js"), ids(&after, "a.js"));
    assert!(
        after
            .calls
            .iter()
            .all(|c| c.revision_id == after.revision.id)
    );
    // Only the edited document is re-identified.
    let (old_b, new_b) = (ids(&before, "b.js"), ids(&after, "b.js"));
    assert!(old_b.iter().all(|id| !new_b.contains(id)));
}

#[test]
fn unverifiable_expression_keeps_independent_nullable_callee() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    fs::write(root.path().join("a.js"), "function go() { obj[key](); }").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let (_graph, native) = index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &id.record_id,
        &cancel,
        |_| {},
    )
    .unwrap();
    let call = native
        .calls
        .iter()
        .find(|call| call.spelling.as_deref() == Some("obj[key]"))
        .unwrap();
    assert_eq!(call.callee_range, None);
    assert_eq!(call.region_ids.len(), 0);
    assert!(!serde_json::to_string(call).unwrap().contains("target"));
    assert!(!serde_json::to_string(call).unwrap().contains("receiver"));
}

#[test]
fn nested_occurrences_sort_before_hashing_and_unverifiable_callees_remain_nullable() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    for (path, source) in [
        (
            "A.java",
            "class A { A() { this(1); } A(int n) {} void go() { obj.foo(); new A(); } }",
        ),
        (
            "a.js",
            "function go() { obj.foo(); obj[key](); foo()(); if (true) { foo(); } }",
        ),
        (
            "a.py",
            "def go():\n    obj.foo()\n    (foo())()\n    if True:\n        foo()\n",
        ),
        ("a.rs", "fn go() { obj.foo(); foo()(); if true { foo(); } }"),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    artifact
        .validate(&capture, root.path(), &identity.record_id, &cancel)
        .unwrap();
    // Re-validation honors the caller's cancellation instead of a private flag.
    let cancelled: CancelFlag = Arc::new(AtomicBool::new(true));
    let error = artifact
        .validate(&capture, root.path(), &identity.record_id, &cancelled)
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    // These are literal source expressions, not records regenerated as the test oracle.
    for (lang, path, unverified_expression, spelling) in [
        ("java", "A.java", "new A()", None),
        ("javascript", "a.js", "obj[key]()", Some("obj[key]")),
        ("python", "a.py", "(foo())()", Some("(foo())")),
        ("rust", "a.rs", "foo()()", Some("foo()")),
    ] {
        let source = &capture
            .files
            .iter()
            .find(|file| file.path == path)
            .unwrap()
            .text;
        let owner = artifact
            .declarations
            .iter()
            .find(|d| d.document.language == lang && d.name.as_deref() == Some("go"))
            .unwrap();
        let positive_at = source.find("obj.foo()").unwrap();
        let positive = artifact
            .calls
            .iter()
            .find(|call| call.document.language == lang && call.range.start == positive_at)
            .unwrap();
        assert_eq!(
            (positive.range.start, positive.range.end),
            (positive_at, positive_at + "obj.foo()".len())
        );
        assert_eq!(positive.spelling.as_deref(), Some("foo"));
        let token = positive.callee_range.as_ref().unwrap();
        assert_eq!((token.start, token.end), (positive_at + 4, positive_at + 7));
        assert_eq!(positive.owner_syntax_id, owner.syntax_id);
        let at = source.find(unverified_expression).unwrap();
        let unverified = artifact
            .calls
            .iter()
            .find(|call| {
                call.document.language == lang
                    && call.range.start == at
                    && call.range.end == at + unverified_expression.len()
            })
            .unwrap();
        assert_eq!(
            unverified.spelling.as_deref(),
            spelling,
            "{lang} full captured spelling"
        );
        assert_eq!(unverified.callee_range, None, "{lang} cannot infer a token");
        assert_eq!(
            unverified.owner_syntax_id, owner.syntax_id,
            "{lang} lexical owner"
        );
        assert_eq!(
            &source[unverified.range.start..unverified.range.end],
            unverified_expression
        );
        let wire = serde_json::to_value(unverified).unwrap();
        for field in [
            "receiver",
            "target",
            "hint",
            "candidateSymbols",
            "resolution",
            "binding",
            "symbol",
        ] {
            assert!(
                wire.get(field).is_none(),
                "{lang} fabricated semantic {field}"
            );
        }
    }
    let native_wire = serde_json::to_value(&artifact).unwrap();
    for field in ["symbols", "bindings", "semanticProducer"] {
        assert!(
            native_wire.get(field).is_none(),
            "native artifact fabricated {field}"
        );
    }
    assert!(
        !artifact.references.is_empty(),
        "native-v9 measures lexical sites"
    );
    for reference in &artifact.references {
        let source = &capture
            .files
            .iter()
            .find(|file| file.path == reference.document.path)
            .unwrap()
            .text;
        assert_eq!(
            &source[reference.range.start..reference.range.end],
            reference.spelling
        );
        let wire = serde_json::to_value(reference).unwrap();
        for field in [
            "site",
            "roles",
            "target",
            "symbol",
            "binding",
            "resolution",
            "candidateSymbols",
        ] {
            assert!(
                wire.get(field).is_none(),
                "native reference fabricated semantic {field}"
            );
        }
    }
    let js: Vec<_> = artifact
        .calls
        .iter()
        .filter(|c| {
            c.document.language == "javascript"
                && c.owner_syntax_id
                    == artifact
                        .declarations
                        .iter()
                        .find(|d| {
                            d.document.language == "javascript" && d.name.as_deref() == Some("go")
                        })
                        .unwrap()
                        .syntax_id
        })
        .collect();
    let nested: Vec<_> = js
        .iter()
        .filter(|c| {
            c.range.start
                == js
                    .iter()
                    .find(|c| c.spelling.as_deref() == Some("foo()"))
                    .unwrap()
                    .range
                    .start
        })
        .collect();
    assert_eq!(nested.len(), 2);
    assert!(nested[0].range.end != nested[1].range.end);
    let a = nested.iter().min_by_key(|c| c.range.end).unwrap();
    let b = nested.iter().max_by_key(|c| c.range.end).unwrap();
    assert!(
        a.ordinal < b.ordinal,
        "inner call must sort before outer at equal start"
    );
}

#[test]
fn every_nullable_native_field_requires_presence_even_when_null() {
    use serde::{Serialize, de::DeserializeOwned};
    use trellis::native_evidence::*;
    fn check<T: Serialize + DeserializeOwned>(row: &T, fields: &[&str]) {
        let baseline = serde_json::to_value(row).unwrap();
        assert!(serde_json::from_value::<T>(baseline.clone()).is_ok());
        for field in fields {
            let mut missing = baseline.clone();
            missing.as_object_mut().unwrap().remove(*field).unwrap();
            assert!(
                serde_json::from_value::<T>(missing).is_err(),
                "missing {field} accepted"
            );
            let mut explicit_null = baseline.clone();
            explicit_null[*field] = serde_json::Value::Null;
            assert!(
                serde_json::from_value::<T>(explicit_null).is_ok(),
                "explicit null {field} rejected"
            );
        }
    }
    let doc = DocumentKey {
        source_set_id: "s".into(),
        language: "rust".into(),
        path: "a.rs".into(),
    };
    check(
        &MeasurementSupport {
            kind: MeasurementKind::Reference,
            available: true,
            diagnostic: None,
            gaps: vec![],
        },
        &["diagnostic"],
    );
    check(
        &Coverage {
            producer_id: "p".into(),
            language: "rust".into(),
            source_set_id: "s".into(),
            document_path: "a.rs".into(),
            revision_id: "r".into(),
            requested: true,
            selected: true,
            state: "complete".into(),
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: None,
        },
        &["diagnostic"],
    );
    check(
        &Provenance {
            id: "proof".into(),
            producer_id: "p".into(),
            document: doc.clone(),
            revision_id: "r".into(),
            content_hash: "h".into(),
            evidence_kind: "measuredSyntax".into(),
            basis: None,
            freshness: "fresh".into(),
            derived_from: None,
        },
        &["basis", "derivedFrom"],
    );
    let key = Key {
        kind: "module".into(),
        name: None,
        signature: None,
        ordinal: 0,
    };
    check(&key, &["name", "signature"]);
    check(
        &Parameter {
            name: None,
            type_name: None,
            variadic: false,
        },
        &["name", "type"],
    );
    let header = Header {
        kind: "module".into(),
        name: None,
        modifiers: vec![],
        type_parameters: vec![],
        parameters: vec![],
        result_type: None,
        bases: vec![],
    };
    check(&header, &["name", "resultType"]);
    check(
        &Declaration {
            role: trellis::native_evidence::DeclarationRole::Production,
            syntax_id: "id".into(),
            document: doc.clone(),
            revision_id: "r".into(),
            kind: "module".into(),
            name: None,
            lookup_key: None,
            ancestors: vec![],
            key,
            range: Range { start: 0, end: 0 },
            name_range: None,
            header,
            provenance_id: "proof".into(),
        },
        &["name", "lookupKey", "nameRange"],
    );
    check(
        &Call {
            id: "id".into(),
            owner_syntax_id: "owner".into(),
            ordinal: 0,
            document: doc.clone(),
            revision_id: "r".into(),
            range: Range { start: 0, end: 1 },
            callee_range: None,
            spelling: None,
            region_ids: vec![],
            provenance_id: "proof".into(),
        },
        &["calleeRange", "spelling"],
    );
    check(
        &ControlRegion {
            id: "id".into(),
            owner_syntax_id: "owner".into(),
            ordinal: 0,
            document: doc,
            revision_id: "r".into(),
            kind: "if_statement".into(),
            range: Range { start: 0, end: 1 },
            parent_id: None,
            arm: None,
            provenance_id: "proof".into(),
        },
        &["parentId", "arm"],
    );
}

#[test]
fn native_validation_rejects_invalid_structure_against_immutable_capture() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture,
        indexer::IndexOptions,
        model::CancelFlag,
        native_evidence::{Artifact, from_capture},
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    fs::write(
        root.path().join("A.java"),
        "class A { void go(int n) { obj.foo(); } }",
    )
    .unwrap();
    fs::write(
        root.path().join("a.js"),
        "function go() { if (true) obj.foo(); }",
    )
    .unwrap();
    fs::write(root.path().join("a.py"), "def go():\n    obj.foo()\n").unwrap();
    fs::write(root.path().join("a.rs"), "fn go() { obj.foo(); }").unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\n",
    )
    .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &id.record_id, &cancel).unwrap();
    let bad_hash = "a".repeat(64);
    type Mutation = (&'static str, Box<dyn Fn(&mut Artifact)>);
    let mut mutations: Vec<Mutation> = vec![
        (
            "producer executable hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.producer.executable_hash = h.clone()
            }),
        ),
        (
            "producer languages",
            Box::new(|a| a.producer.languages.swap(0, 1)),
        ),
        (
            "source set root",
            Box::new(|a| a.source_set.root_id = "forged".into()),
        ),
        (
            "source set dependencies",
            Box::new(|a| a.source_set.dependencies.push("forged".into())),
        ),
        (
            "revision id",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.revision.id = format!("revision:v2:{h}")
            }),
        ),
        (
            "toolchain hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.revision.toolchain_hash = h.clone()
            }),
        ),
        (
            "config hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.revision.config_hash = h.clone()
            }),
        ),
        (
            "dependency hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.revision.dependency_hash = h.clone()
            }),
        ),
        (
            "document content hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.revision.documents[0].content_hash = h.clone()
            }),
        ),
        (
            "document bytes length",
            Box::new(|a| a.revision.documents[0].byte_length += 1),
        ),
        (
            "document order",
            Box::new(|a| a.revision.documents.swap(0, 1)),
        ),
        (
            "provenance proof document",
            Box::new(|a| a.provenance[0].document = a.provenance[1].document.clone()),
        ),
        (
            "provenance content hash",
            Box::new({
                let h = bad_hash.clone();
                move |a| a.provenance[0].content_hash = h.clone()
            }),
        ),
        (
            "provenance basis",
            Box::new(|a| a.provenance[0].basis = Some(serde_json::json!({}))),
        ),
        (
            "declaration signature",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.document.language == "java" && d.name.as_deref() == Some("go"))
                    .unwrap()
                    .key
                    .signature
                    .as_mut()
                    .unwrap()
                    .parameter_types[0] = "String".into()
            }),
        ),
        (
            "declaration ancestor",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.document.language == "java" && d.name.as_deref() == Some("go"))
                    .unwrap()
                    .ancestors
                    .clear()
            }),
        ),
        (
            "declaration name range",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.name.as_deref() == Some("go"))
                    .unwrap()
                    .name_range
                    .as_mut()
                    .unwrap()
                    .start += 1
            }),
        ),
        ("call ordinal", Box::new(|a| a.calls[0].ordinal += 1)),
        (
            "call region",
            Box::new(|a| a.calls[0].region_ids.push("not-a-region".into())),
        ),
        (
            "control owner",
            Box::new(|a| {
                a.control_regions[0].owner_syntax_id = a.declarations[0].syntax_id.clone()
            }),
        ),
        (
            "reference ordinal",
            Box::new(|a| a.references[0].ordinal += 1),
        ),
        (
            "reference spelling",
            Box::new(|a| a.references[0].spelling.push('x')),
        ),
        (
            "reference owner",
            Box::new(|a| a.references[0].owner_syntax_id = "missing".into()),
        ),
        (
            "reference duplicate",
            Box::new(|a| a.references.push(a.references[0].clone())),
        ),
        (
            "capability missing",
            Box::new(|a| {
                a.capability_reports.pop();
            }),
        ),
        (
            "capability duplicate family",
            Box::new(|a| {
                let family = a.capability_reports[0].families[0].clone();
                a.capability_reports[0].families.push(family);
            }),
        ),
        (
            "capability contradicts candidate",
            Box::new(|a| {
                let reference = a.references[0].clone();
                let report = a
                    .capability_reports
                    .iter_mut()
                    .find(|r| r.document == reference.document)
                    .unwrap();
                let family = report
                    .families
                    .iter_mut()
                    .find(|f| f.kind == trellis::native_evidence::MeasurementKind::Reference)
                    .unwrap();
                family.gaps.push(trellis::native_evidence::MeasurementGap {
                    range: reference.range,
                    diagnostic: "invented gap".into(),
                });
            }),
        ),
    ];
    for (label, mutator) in mutations.drain(..) {
        let mut changed = artifact.clone();
        mutator(&mut changed);
        assert!(
            changed
                .validate(&capture, root.path(), &id.record_id, &cancel)
                .is_err(),
            "forged {label} accepted"
        );
    }
    // #225 keeps structural validation, not a second extraction to compare facts.
    // These edits remain structurally valid and no longer require re-derivation.
    let accepted: Vec<Mutation> = vec![
        (
            "coverage complete roles",
            Box::new(|a| a.coverage[0].observed_roles.clear()),
        ),
        (
            "coverage failed with diagnostic",
            Box::new(|a| {
                a.coverage[0].state = "failed".into();
                a.coverage[0].diagnostic = Some("failed".into());
            }),
        ),
        (
            "coverage partial with diagnostic",
            Box::new(|a| {
                a.coverage[0].state = "partial".into();
                a.coverage[0].diagnostic = Some("partial".into());
            }),
        ),
        (
            "declaration modifier",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.name.as_deref() == Some("go"))
                    .unwrap()
                    .header
                    .modifiers
                    .push("public".into())
            }),
        ),
        (
            "declaration result type",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.document.language == "java" && d.name.as_deref() == Some("go"))
                    .unwrap()
                    .header
                    .result_type = Some("String".into())
            }),
        ),
        (
            "declaration source range",
            Box::new(|a| {
                a.declarations
                    .iter_mut()
                    .find(|d| d.name.as_deref() == Some("go"))
                    .unwrap()
                    .range
                    .end -= 1
            }),
        ),
        (
            "call unverified spelling",
            Box::new(|a| {
                let c = a
                    .calls
                    .iter_mut()
                    .find(|c| c.document.language == "javascript")
                    .unwrap();
                c.spelling = Some("guessed".into());
                c.callee_range = None;
            }),
        ),
        (
            "control kind",
            Box::new(|a| a.control_regions[0].kind = "for_statement".into()),
        ),
        (
            "control arm",
            Box::new(|a| a.control_regions[0].arm = Some("fabricated".into())),
        ),
        (
            "control range",
            Box::new(|a| a.control_regions[0].range.end += 1),
        ),
    ];
    for (label, mutator) in accepted {
        let mut changed = artifact.clone();
        mutator(&mut changed);
        changed
            .validate(&capture, root.path(), &id.record_id, &cancel)
            .unwrap_or_else(|error| panic!("structurally valid {label} refused: {error:#}"));
    }
    artifact
        .validate(&capture, root.path(), &id.record_id, &cancel)
        .unwrap();
    for ops in capture.source_operations.values() {
        assert_eq!((ops.opens, ops.complete_reads, ops.hashes), (1, 1, 1));
    }
}

#[test]
fn native_validation_rejects_cross_document_proof_for_every_fact_family() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture,
        indexer::IndexOptions,
        model::CancelFlag,
        native_evidence::{Artifact, from_capture},
    };
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        ("a.js", "function first(value) { if (value) { one(); } }"),
        ("b.js", "function second(value) { if (value) { two(); } }"),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    artifact
        .validate(&capture, root.path(), &identity.record_id, &cancel)
        .unwrap();
    type Mutation = (&'static str, fn(&mut Artifact));
    let mutations: [Mutation; 4] = [
        ("declaration", |a| {
            a.declarations[0].provenance_id = a
                .provenance
                .iter()
                .find(|p| p.document != a.declarations[0].document)
                .unwrap()
                .id
                .clone();
        }),
        ("call", |a| {
            a.calls[0].provenance_id = a
                .provenance
                .iter()
                .find(|p| p.document != a.calls[0].document)
                .unwrap()
                .id
                .clone();
        }),
        ("reference", |a| {
            a.references[0].provenance_id = a
                .provenance
                .iter()
                .find(|p| p.document != a.references[0].document)
                .unwrap()
                .id
                .clone();
        }),
        ("control region", |a| {
            a.control_regions[0].provenance_id = a
                .provenance
                .iter()
                .find(|p| p.document != a.control_regions[0].document)
                .unwrap()
                .id
                .clone();
        }),
    ];
    for (family, mutate) in mutations {
        let mut changed = artifact.clone();
        mutate(&mut changed);
        assert!(
            changed
                .validate(&capture, root.path(), &identity.record_id, &cancel)
                .is_err(),
            "{family} accepted a proof belonging to another document"
        );
    }
}

#[test]
fn parser_recovery_reports_partial_not_false_complete() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    fs::write(root.path().join("broken.js"), "function broken( { foo(); }").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &id.record_id, &cancel).unwrap();
    assert_eq!(artifact.coverage.len(), 1);
    assert_eq!(artifact.coverage[0].state, "partial");
    assert!(artifact.coverage[0].diagnostic.is_some());
    let mut forged = artifact.clone();
    // A complete coverage row cannot retain the partial diagnostic.
    forged.coverage[0].state = "complete".into();
    assert!(
        forged
            .validate(&capture, root.path(), &id.record_id, &cancel)
            .is_err()
    );
}

#[test]
fn validation_refuses_cutoff_drift_without_source_reread() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    let source = root.path().join("a.js");
    fs::write(&source, "function f() { foo(); }").unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &id.record_id, &cancel).unwrap();
    fs::write(&source, "function f() { bar(); }").unwrap();
    assert!(
        artifact
            .validate(&capture, root.path(), &id.record_id, &cancel)
            .is_err()
    );
    assert_eq!(capture.source_operations["a.js"].complete_reads, 1);
    assert_eq!(capture.source_operations["a.js"].hashes, 1);
}

#[test]
fn source_grounded_four_language_headers_owners_and_control_regions() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    for (path, source) in [
        (
            "A.java",
            "public class A { public static void go() { if (true) obj.foo(); } }",
        ),
        ("a.js", "async function go() { if (true) obj.foo(); }"),
        ("a.py", "async def go():\n    if True:\n        obj.foo()\n"),
        (
            "a.rs",
            "pub struct S {}\nstruct Plain {}\npub async fn go() { if true { obj.foo(); } }\nfn plain() {}",
        ),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &id.record_id, &cancel).unwrap();
    let rust_struct = artifact
        .declarations
        .iter()
        .find(|d| d.document.language == "rust" && d.name.as_deref() == Some("S"))
        .unwrap();
    assert_eq!(rust_struct.header.modifiers, ["pub"]);
    let rust_plain = artifact
        .declarations
        .iter()
        .find(|d| d.document.language == "rust" && d.name.as_deref() == Some("Plain"))
        .unwrap();
    assert!(
        rust_plain.header.modifiers.is_empty(),
        "private Rust struct must not inherit pub"
    );
    let plain_function = artifact
        .declarations
        .iter()
        .find(|d| d.document.language == "rust" && d.name.as_deref() == Some("plain"))
        .unwrap();
    assert!(
        plain_function.header.modifiers.is_empty(),
        "plain Rust fn must not inherit async/pub"
    );
    let java_type = artifact
        .declarations
        .iter()
        .find(|d| d.document.language == "java" && d.name.as_deref() == Some("A"))
        .unwrap();
    assert_eq!(java_type.header.modifiers, ["public"]);
    for (lang, path, modifiers, control_text, kind) in [
        (
            "java",
            "A.java",
            vec!["public", "static"],
            "if (true) obj.foo();",
            "if_statement",
        ),
        (
            "javascript",
            "a.js",
            vec!["async"],
            "if (true) obj.foo();",
            "if_statement",
        ),
        (
            "python",
            "a.py",
            vec!["async"],
            "if True:\n        obj.foo()",
            "if_statement",
        ),
        (
            "rust",
            "a.rs",
            vec!["pub", "async"],
            "if true { obj.foo(); }",
            "if_expression",
        ),
    ] {
        let source = &capture.files.iter().find(|f| f.path == path).unwrap().text;
        let owner = artifact
            .declarations
            .iter()
            .find(|d| d.document.language == lang && d.name.as_deref() == Some("go"))
            .unwrap();
        assert_eq!(
            owner.header.modifiers, modifiers,
            "{lang} literal header modifiers"
        );
        assert_eq!(
            &source[owner.range.start..owner.range.end].contains("go"),
            &true
        );
        if lang == "java" {
            assert_eq!(owner.header.result_type.as_deref(), Some("void"));
        }
        let begin = source.find(control_text).unwrap();
        let region = artifact
            .control_regions
            .iter()
            .find(|r| r.document.language == lang && r.kind == kind && r.range.start == begin)
            .unwrap_or_else(|| panic!("missing {lang} control: {:?}", artifact.control_regions));
        assert_eq!(
            (region.range.start, region.range.end),
            (begin, begin + control_text.len())
        );
        assert_eq!(region.owner_syntax_id, owner.syntax_id);
        let call_at = source.find("obj.foo()").unwrap();
        let call = artifact
            .calls
            .iter()
            .find(|c| c.document.language == lang && c.range.start == call_at)
            .unwrap();
        assert_eq!(call.owner_syntax_id, owner.syntax_id);
        assert!(call.region_ids.contains(&region.id));
        assert_eq!(call.spelling.as_deref(), Some("foo"));
        assert_eq!(call.callee_range.as_ref().unwrap().start, call_at + 4);
    }
}

#[test]
fn direct_native_opaque_macro_does_not_overflow_or_invent_calls() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        "macro_rules! generated {{ () => {{ fn invented() {{ hidden(); }} }}; }}\n#[cfg(feature = \"unknown\")]\nfn run() {{ opaque!({} Fake::new().a().b() {}); actual(); }}",
        "(".repeat(512),
        ")".repeat(512)
    );
    fs::write(root.path().join("a.rs"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    artifact
        .validate(&capture, root.path(), &identity.record_id, &cancel)
        .unwrap();
    let spellings: Vec<_> = artifact
        .calls
        .iter()
        .filter_map(|call| call.spelling.as_deref())
        .collect();
    assert_eq!(spellings, ["actual"]);
    assert_eq!(artifact.coverage.len(), 1);
    let coverage = &artifact.coverage[0];
    assert!(coverage.requested && coverage.selected);
    assert_eq!(coverage.state, "partial");
    assert_eq!(coverage.supported_roles, ["definition", "call"]);
    assert_eq!(coverage.observed_roles, ["definition", "call"]);
    assert!(
        coverage
            .diagnostic
            .as_deref()
            .unwrap()
            .contains("opaque Rust")
    );
}

#[test]
fn nonopaque_rust_nesting_retains_fail_closed_depth_guard() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let root = tempfile::tempdir().unwrap();
    let source = format!(
        "fn run() {{ {} actual(); {} }}",
        "{".repeat(150),
        "}".repeat(150)
    );
    fs::write(root.path().join("deep.rs"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let error = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("native extraction work budget exceeded"),
        "{error:#}"
    );
}

#[test]
fn max_file_bytes_and_previously_absent_required_input_change_native_basis() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let first_options = IndexOptions::new(root.path().to_owned());
    let (_, first) =
        index_workspace_with_native(&first_options, &identity.record_id, &cancel, |_| {}).unwrap();
    let mut changed_cap = first_options.clone();
    changed_cap.max_file_bytes += 1;
    let (_, second) =
        index_workspace_with_native(&changed_cap, &identity.record_id, &cancel, |_| {}).unwrap();
    assert_ne!(first.revision.config_hash, second.revision.config_hash);
    assert_ne!(first.revision.id, second.revision.id);
    assert_eq!(
        first.revision.dependency_hash,
        second.revision.dependency_hash
    );

    fs::write(root.path().join("package.json"), "{\"name\":\"new\"}\n").unwrap();
    let (_, third) =
        index_workspace_with_native(&changed_cap, &identity.record_id, &cancel, |_| {}).unwrap();
    assert_ne!(
        second.revision.dependency_hash,
        third.revision.dependency_hash
    );
    assert_ne!(second.revision.id, third.revision.id);
}

#[cfg(unix)]
#[test]
fn required_inputs_are_rehashed_when_stats_look_unchanged_and_ignore_changes_are_native_config() {
    use std::{
        fs,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let package = root.path().join("package.json");
    fs::write(&package, "{\"name\":\"aaaa\"}\n").unwrap();
    let ignore = root.path().join(".gitignore");
    fs::write(&ignore, "first.js\n").unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let options = IndexOptions::new(root.path().to_owned());
    let (_, first) =
        index_workspace_with_native(&options, &identity.record_id, &cancel, |_| {}).unwrap();

    let metadata = fs::metadata(&package).unwrap();
    fs::write(&package, "{\"name\":\"bbbb\"}\n").unwrap();
    let c_path = std::ffi::CString::new(package.as_os_str().as_bytes()).unwrap();
    let times = [
        libc::timespec {
            tv_sec: metadata.atime(),
            tv_nsec: metadata.atime_nsec(),
        },
        libc::timespec {
            tv_sec: metadata.mtime(),
            tv_nsec: metadata.mtime_nsec(),
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) },
        0
    );
    let (_, second) =
        index_workspace_with_native(&options, &identity.record_id, &cancel, |_| {}).unwrap();
    assert_ne!(
        first.revision.dependency_hash,
        second.revision.dependency_hash
    );
    assert_ne!(first.revision.id, second.revision.id);

    fs::write(&ignore, "second.js\n").unwrap();
    let (_, third) =
        index_workspace_with_native(&options, &identity.record_id, &cancel, |_| {}).unwrap();
    assert_ne!(second.revision.config_hash, third.revision.config_hash);
    assert_ne!(second.revision.id, third.revision.id);
}

#[test]
fn unselected_scip_files_do_not_change_native_revision_identity() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let scip = root.path().join("labels.scip");
    let manifest = root.path().join("labels.json");
    fs::write(&scip, b"first presentation").unwrap();
    fs::write(&manifest, b"{}").unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let (_, first) =
        index_workspace_with_native(&options, &identity.record_id, &cancel, |_| {}).unwrap();
    fs::write(&scip, b"second presentation").unwrap();
    fs::write(&manifest, b"{\"changed\":\"yes\"}").unwrap();
    let (_, second) =
        index_workspace_with_native(&options, &identity.record_id, &cancel, |_| {}).unwrap();
    assert_eq!(first.revision, second.revision);
}

#[cfg(unix)]
#[test]
fn unreadable_required_input_fails_closed() {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), "function a() {}\n").unwrap();
    let package = root.path().join("package.json");
    fs::write(&package, "{}").unwrap();
    fs::set_permissions(&package, fs::Permissions::from_mode(0o000)).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let result = index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &cancel,
        |_| {},
    );
    fs::set_permissions(&package, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        result.is_err(),
        "unreadable required input was treated as absent"
    );
}

#[test]
fn declaration_roles_follow_syntax_paths_and_inherit_without_changing_identity() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_with_native},
        model::CancelFlag,
        native_evidence::DeclarationRole,
    };
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("tests")).unwrap();
    fs::create_dir(root.path().join("__tests__")).unwrap();
    let id = trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
        .unwrap();
    let sources = [
        (
            "src.rs",
            "fn normal() {}\n#[cfg(test)] mod inner { fn helper() {} struct Thing; impl Thing { fn method(&self) {} } }\nfn after() {}\n#[cfg(all(feature = \"x\", test))] fn all_test() {}\n#[cfg(any(test, feature = \"x\"))] fn any_test() {}\n#[cfg(not(test))] fn not_test() {}\n#[cfg(feature = \"test-support\")] fn support() {}\n#[test] fn standalone() {}\n#[tokio::test] fn qualified() {}\n#[cfg(test)] /* note */ fn after_block_comment() {}\nfn unmarked_after_comment() {}\n// #[test] fn commented() {}\nconst TEXT: &str = \"#[test]\";\n#[tokio::test(flavor = \"multi_thread\")] fn with_args() {}\n#[serde(rename = \"test\")] fn serde_named() {}\nmod gated { #![cfg(test)] fn gated_helper() {} }\n#[tokio::test // note\n(flavor = \"multi_thread\")] fn commented_args() {}\n#[cfg( // note\n test)] fn commented_cfg() {}\n",
        ),
        (
            "inner_commented.rs",
            "#![cfg( // note\n test)]\nfn commented_file_helper() {}\n",
        ),
        (
            "inner.rs",
            "#![cfg(test)]\nfn file_helper() {}\nstruct FileThing;\n",
        ),
        (
            "inner_any.rs",
            "#![cfg(any(test, feature = \"x\"))]\nfn maybe() {}\n",
        ),
        (
            "A.java",
            "class A { @Test void outside() {} @org.testng.annotations.Test void testng() {} @org.junit.jupiter.params.ParameterizedTest void parameterized() {} @RepeatedTest void repeated() {} @TestFactory void factory() {} @TestTemplate void template() {} void normal() {} }",
        ),
        ("tests/Widget.java", "class Widget { void ordinary() {} }"),
        (
            "tests/checks.rs",
            "struct Helper; fn ordinary() { let value = 1; }",
        ),
        (
            "test_sample.py",
            "class TestSuite:\n    def helper(self): pass\nasync def test_async(): pass\ndef test_free(): pass\ndef normal(): pass\n",
        ),
        (
            "normal.py",
            "def test_named(): pass\nclass TestNamed: pass\n",
        ),
        (
            "thing.js",
            "function test_name() { describe('x', () => {}); }",
        ),
        ("__tests__/thing.js", "function ordinary() {}"),
    ];
    for (name, text) in sources {
        fs::write(root.path().join(name), text).unwrap();
    }
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let extract = || {
        index_workspace_with_native(
            &IndexOptions::new(root.path().into()),
            &id.record_id,
            &cancel,
            |_| {},
        )
        .unwrap()
        .1
    };
    let before = extract();
    // Exhaustive source-authored expectations include every synthetic, anonymous,
    // nested and non-callable row, not only name-bearing functions.
    let expected = [
        (
            "A.java",
            DeclarationRole::Production,
            &["module:", "type:A", "method:normal"][..],
        ),
        (
            "A.java",
            DeclarationRole::Test,
            &[
                "method:outside",
                "method:testng",
                "method:parameterized",
                "method:repeated",
                "method:factory",
                "method:template",
            ],
        ),
        (
            "tests/Widget.java",
            DeclarationRole::Test,
            &["module:", "type:Widget", "method:ordinary"],
        ),
        (
            "src.rs",
            DeclarationRole::Production,
            &[
                "module:",
                "function:normal",
                "function:after",
                "function:any_test",
                "function:not_test",
                "function:support",
                "function:unmarked_after_comment",
                "variable:TEXT",
                "function:serde_named",
            ],
        ),
        (
            "src.rs",
            DeclarationRole::Test,
            &[
                "namespace:inner",
                "function:helper",
                "type:Thing",
                "implementation:",
                "method:method",
                "function:all_test",
                "function:standalone",
                "function:qualified",
                "function:after_block_comment",
                "function:with_args",
                "namespace:gated",
                "function:gated_helper",
                "function:commented_args",
                "function:commented_cfg",
            ],
        ),
        (
            "inner_commented.rs",
            DeclarationRole::Test,
            &["module:", "function:commented_file_helper"],
        ),
        (
            "inner.rs",
            DeclarationRole::Test,
            &["module:", "function:file_helper", "type:FileThing"],
        ),
        (
            "inner_any.rs",
            DeclarationRole::Production,
            &["module:", "function:maybe"],
        ),
        (
            "tests/checks.rs",
            DeclarationRole::Test,
            &["module:", "type:Helper", "function:ordinary"],
        ),
        (
            "normal.py",
            DeclarationRole::Production,
            &["module:", "function:test_named", "type:TestNamed"],
        ),
        (
            "test_sample.py",
            DeclarationRole::Test,
            &[
                "module:",
                "type:TestSuite",
                "function:helper",
                "function:test_async",
                "function:test_free",
                "function:normal",
            ],
        ),
        (
            "__tests__/thing.js",
            DeclarationRole::Test,
            &["module:", "function:ordinary"],
        ),
        (
            "thing.js",
            DeclarationRole::Production,
            &["module:", "function:test_name", "anonymousFunction:"],
        ),
    ];
    let mut expected_rows = expected
        .iter()
        .flat_map(|(path, role, rows)| {
            rows.iter().map(move |row| {
                let (kind, name) = row.split_once(':').unwrap();
                (
                    (*path).to_owned(),
                    kind.to_owned(),
                    (!name.is_empty()).then(|| name.to_owned()),
                    serde_json::to_string(role).unwrap(),
                )
            })
        })
        .collect::<Vec<_>>();
    let mut actual_rows = before
        .declarations
        .iter()
        .map(|d| {
            (
                d.document.path.clone(),
                d.kind.clone(),
                d.name.clone(),
                serde_json::to_string(&d.role).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    expected_rows.sort();
    actual_rows.sort();
    assert_eq!(
        actual_rows, expected_rows,
        "every emitted declaration has the source-expected role"
    );
    assert_eq!(before.declarations.len(), expected_rows.len());
    // An attribute changes the measured role, never the stable declaration identity.
    let identities = |artifact: &trellis::native_evidence::Artifact| {
        let mut rows = artifact
            .declarations
            .iter()
            .map(|d| {
                (
                    serde_json::to_string(&d.document).unwrap(),
                    d.syntax_id.clone(),
                    serde_json::to_string(&d.key).unwrap(),
                    d.lookup_key.clone(),
                )
            })
            .collect::<Vec<_>>();
        rows.sort();
        rows
    };
    let baseline = identities(&before);
    assert_eq!(baseline.len(), before.declarations.len());
    let original = fs::read_to_string(root.path().join("src.rs")).unwrap();
    fs::write(
        root.path().join("src.rs"),
        original.replace("#[cfg(test)]", "#[cfg(any(test))]"),
    )
    .unwrap();
    let after = extract();
    let changed = identities(&after);
    assert_eq!(changed.len(), after.declarations.len());
    assert_eq!(baseline, changed);
    assert_eq!(
        after
            .declarations
            .iter()
            .find(|d| d.document.path == "src.rs" && d.name.as_deref() == Some("helper"))
            .unwrap()
            .role,
        DeclarationRole::Production
    );
}

#[test]
fn verified_callee_keys_decode_only_complete_measured_tokens() {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture,
        indexer::IndexOptions,
        model::CancelFlag,
        native_evidence::{from_capture, verified_callee_lookup},
    };
    let root = tempfile::tempdir().unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    for (path, text) in [
        (
            "A.java",
            "class A { void foo() {} void go() { obj.foo(); (obj.foo())(); } }",
        ),
        (
            "a.rs",
            "fn r#match() {} fn go() { obj.r#match(); (foo())(); }",
        ),
        (
            "a.py",
            "def foo():\n    pass\ndef go():\n    obj.foo()\n    (foo())()\n",
        ),
        (
            "a.js",
            r"function \u{54}arget() {} function go() { obj.\u{54}arget(); obj.\u0054arget(); obj[Target](); }",
        ),
    ] {
        fs::write(root.path().join(path), text).unwrap();
    }
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    for (path, spelling, expected) in [
        ("A.java", "foo", "foo"),
        ("a.rs", "r#match", "match"),
        ("a.py", "foo", "foo"),
        ("a.js", r"\u{54}arget", "Target"),
    ] {
        let source = capture.files.iter().find(|f| f.path == path).unwrap();
        let call = artifact
            .calls
            .iter()
            .find(|c| {
                c.document.path == path
                    && c.spelling.as_deref() == Some(spelling)
                    && c.callee_range.is_some()
            })
            .unwrap();
        assert_eq!(
            verified_callee_lookup(&source.language, call, &source.text).as_deref(),
            Some(expected),
            "{path}"
        );
        let mut altered = call.clone();
        altered.spelling = Some(format!("obj.{spelling}"));
        assert_eq!(
            verified_callee_lookup(&source.language, &altered, &source.text),
            None
        );
        assert_eq!(
            &source.text[call.callee_range.as_ref().unwrap().start
                ..call.callee_range.as_ref().unwrap().end],
            spelling
        );
    }
    let js = capture.files.iter().find(|f| f.path == "a.js").unwrap();
    let four_digit = artifact
        .calls
        .iter()
        .find(|c| c.document.path == "a.js" && c.spelling.as_deref() == Some(r"\u0054arget"))
        .unwrap();
    assert_eq!(
        verified_callee_lookup("javascript", four_digit, &js.text).as_deref(),
        Some("Target")
    );
    for (path, name, key) in [
        ("a.rs", "r#match", "match"),
        ("a.js", r"\u{54}arget", "Target"),
    ] {
        let declaration = artifact
            .declarations
            .iter()
            .find(|d| d.document.path == path && d.name.as_deref() == Some(name))
            .unwrap();
        assert_eq!(declaration.lookup_key.as_deref(), Some(key));
    }
}

fn assert_javascript_declaration_lookup(source: &str, name: &str, key: &str) {
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };

    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("a.js"), source).unwrap();
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    // from_capture structurally validates its own build. Store publication
    // and incoming reads are exercised separately.
    let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
    let declaration = artifact
        .declarations
        .iter()
        .find(|d| d.document.path == "a.js" && d.name.as_deref() == Some(name))
        .expect("parser-measured escaped declaration");
    assert_eq!(declaration.lookup_key.as_deref(), Some(key));
    let range = declaration.name_range.as_ref().unwrap();
    assert_eq!(&source[range.start..range.end], name);
}

#[test]
fn javascript_leading_zero_escape_declaration_decodes_for_matching() {
    assert_javascript_declaration_lookup(
        r"function \u{0000054}arget() {}",
        r"\u{0000054}arget",
        "Target",
    );
}

#[test]
fn javascript_escaped_reserved_method_declaration_keeps_raw_key() {
    assert_javascript_declaration_lookup(
        r"class A { d\u0065lete() {} }",
        r"d\u0065lete",
        r"d\u0065lete",
    );
}

#[test]
fn javascript_brace_escape_accepts_leading_zeros_and_bounds_code_point() {
    use trellis::native_evidence::lookup;
    assert_eq!(
        lookup("javascript", r"\u{00000000000000000054}arget").unwrap(),
        "Target"
    );
    assert!(lookup("javascript", r"\u{110000}arget").is_err());
    assert!(lookup("javascript", r"\u{0000000000110000}arget").is_err());
}

/// #199 I-4b: a fresh build is no longer rebuilt and compared inside
/// validation, so `build_native` determinism is checked here instead. The
/// same capture built twice must give byte-identical artifacts.
#[test]
fn native_build_is_deterministic() {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{
        capture::Capture, indexer::IndexOptions, model::CancelFlag, native_evidence::from_capture,
    };
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/metrics");
    let root = tempfile::tempdir().unwrap();
    let mut copied = 0;
    for language in ["java", "javascript", "python", "rust"] {
        std::fs::create_dir(root.path().join(language)).unwrap();
        for entry in std::fs::read_dir(fixtures.join(language)).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(
                entry.path(),
                root.path().join(language).join(entry.file_name()),
            )
            .unwrap();
            copied += 1;
        }
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    assert_eq!(capture.files.len(), copied, "every fixture is captured");
    let build = || {
        let artifact = from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap();
        serde_json::to_vec(&artifact).unwrap()
    };
    let first = build();
    let second = build();
    assert!(
        first.len() > 10_000,
        "fixture exercises real extraction: {} bytes",
        first.len()
    );
    assert!(first == second, "the same capture built twice differs");
}

fn native_v9_fixture(files: &[(&str, &str)]) -> trellis::native_evidence::Artifact {
    use std::sync::{Arc, atomic::AtomicBool};
    use trellis::{capture::Capture, indexer::IndexOptions, native_evidence::from_capture};
    let root = tempfile::tempdir().unwrap();
    for (path, source) in files {
        std::fs::write(root.path().join(path), source).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&IndexOptions::new(root.path().into()), &cancel, &|_| {}).unwrap();
    from_capture(&capture, root.path(), &identity.record_id, &cancel).unwrap()
}

#[test]
fn native_v9_identifier_candidates_are_source_measured_owned_and_ordered() {
    let fixtures = [
        (
            "A.java",
            "import p.Type; class A { Type café; void run(Type value) { café = value.member; use(this::run); } } // excluded\n",
        ),
        (
            "a.rs",
            "use p::Original as Local; use p::{Other as Nested}; fn run(value: Type) { let café = value.member; use_it(café); } // excluded\n",
        ),
        (
            "a.py",
            "import original as local\ndef run(value: Type):\n    café = value.member\n    use_it(café)\n    return 'excluded'\n",
        ),
        (
            "a.js",
            "import {original as local} from 'module'; function run(value) { const café = value.member; use_it(café); const obj = {café}; return `excluded ${value}`; } // excluded\n",
        ),
    ];
    let artifact = native_v9_fixture(&fixtures);
    for (path, source) in fixtures {
        let rows: Vec<_> = artifact
            .references
            .iter()
            .filter(|r| r.document.path == path)
            .collect();
        assert!(rows.iter().any(|r| r.spelling == "café"), "{path}");
        assert!(rows.iter().any(|r| r.spelling == "member"), "{path}");
        assert!(
            !rows
                .iter()
                .any(|r| r.spelling == "excluded" || r.spelling == "module"),
            "{path}"
        );
        for r in &rows {
            assert_eq!(&source[r.range.start..r.range.end], r.spelling);
            assert_eq!(
                trellis::native_evidence::lookup(&r.document.language, &r.spelling).unwrap(),
                r.lookup_key
            );
            assert!(
                artifact
                    .declarations
                    .iter()
                    .any(|d| d.syntax_id == r.owner_syntax_id && d.document == r.document)
            );
        }
        let mut owners = std::collections::BTreeMap::<_, Vec<_>>::new();
        for r in rows {
            owners.entry(&r.owner_syntax_id).or_default().push(r);
        }
        for rows in owners.values_mut() {
            rows.sort_by_key(|r| (r.range.start, r.range.end));
            for (ordinal, r) in rows.iter().enumerate() {
                assert_eq!(r.ordinal, ordinal);
            }
        }
        let module = artifact
            .declarations
            .iter()
            .find(|d| d.document.path == path && d.kind == "module")
            .unwrap();
        let function = artifact
            .declarations
            .iter()
            .find(|d| d.document.path == path && d.name.as_deref() == Some("run"))
            .unwrap();
        let declaration_name = artifact
            .references
            .iter()
            .find(|r| r.document.path == path && Some(&r.range) == function.name_range.as_ref())
            .unwrap();
        let parent = if path == "A.java" {
            artifact
                .declarations
                .iter()
                .find(|d| d.document.path == path && d.name.as_deref() == Some("A"))
                .unwrap()
        } else {
            module
        };
        assert_eq!(declaration_name.owner_syntax_id, parent.syntax_id);
        let use_site = artifact
            .references
            .iter()
            .find(|r| r.document.path == path && r.spelling == "member")
            .unwrap();
        assert_eq!(use_site.owner_syntax_id, function.syntax_id);
    }
    for (path, aliases) in [("a.js", vec!["local"]), ("a.rs", vec!["Local", "Nested"])] {
        for alias in aliases {
            let declaration = artifact
                .declarations
                .iter()
                .find(|d| {
                    d.document.path == path && d.kind == "alias" && d.name.as_deref() == Some(alias)
                })
                .unwrap();
            assert!(declaration.key.signature.is_none());
            let reference = artifact
                .references
                .iter()
                .find(|r| {
                    r.document == declaration.document
                        && Some(&r.range) == declaration.name_range.as_ref()
                })
                .unwrap();
            let module = artifact
                .declarations
                .iter()
                .find(|d| d.document == declaration.document && d.kind == "module")
                .unwrap();
            assert_eq!(reference.owner_syntax_id, module.syntax_id);
        }
    }
}

#[test]
fn native_v9_reference_ownership_does_not_move_initializer_calls() {
    let artifact = native_v9_fixture(&[(
        "a.js",
        "function outer() { const local = schedule(callback); function inner(value) { return value.member; } return inner(local); }",
    )]);
    let outer = artifact
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("outer"))
        .unwrap();
    let inner = artifact
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("inner"))
        .unwrap();
    let inner_name = artifact
        .references
        .iter()
        .find(|r| Some(&r.range) == inner.name_range.as_ref())
        .unwrap();
    assert_eq!(inner_name.owner_syntax_id, outer.syntax_id);
    let member = artifact
        .references
        .iter()
        .find(|r| r.spelling == "member")
        .unwrap();
    assert_eq!(member.owner_syntax_id, inner.syntax_id);
    let callback = artifact
        .references
        .iter()
        .find(|r| r.spelling == "callback")
        .unwrap();
    assert_eq!(callback.owner_syntax_id, outer.syntax_id);
    assert!(
        !artifact
            .calls
            .iter()
            .any(|c| c.spelling.as_deref() == Some("callback"))
    );
    let schedule = artifact
        .calls
        .iter()
        .find(|c| c.spelling.as_deref() == Some("schedule"))
        .unwrap();
    assert_eq!(schedule.owner_syntax_id, outer.syntax_id);
}

#[test]
fn native_v9_rust_gaps_preserve_working_families_and_exclude_non_identifiers() {
    use trellis::native_evidence::MeasurementKind;
    let source = "#[opaque(hidden)] fn run<'life>(value: &'life Type) { 'label: loop { real(value.field); opaque!(invented()); break 'label; } }";
    let artifact = native_v9_fixture(&[("a.rs", source), ("empty.js", "")]);
    assert!(
        artifact
            .calls
            .iter()
            .any(|c| c.spelling.as_deref() == Some("real"))
    );
    assert!(
        !artifact
            .calls
            .iter()
            .any(|c| c.spelling.as_deref() == Some("invented"))
    );
    for excluded in ["life", "label", "hidden", "invented", "opaque"] {
        assert!(
            !artifact.references.iter().any(|r| r.spelling == excluded),
            "{excluded}"
        );
    }
    let report = artifact
        .capability_reports
        .iter()
        .find(|r| r.document.path == "a.rs")
        .unwrap();
    assert_eq!(report.families.len(), 4);
    for family in &report.families {
        assert!(family.available && family.diagnostic.is_none());
        assert_eq!(family.gaps.len(), 2);
        assert_eq!(
            &source[family.gaps[0].range.start..family.gaps[0].range.end],
            "#[opaque(hidden)]"
        );
        assert_eq!(
            &source[family.gaps[1].range.start..family.gaps[1].range.end],
            "opaque!(invented())"
        );
        assert!(family.gaps.iter().all(|gap| !gap.diagnostic.is_empty()));
    }
    assert!(
        report
            .families
            .iter()
            .any(|f| f.kind == MeasurementKind::Reference)
    );
    let empty = artifact
        .capability_reports
        .iter()
        .find(|r| r.document.path == "empty.js")
        .unwrap();
    assert!(
        empty
            .families
            .iter()
            .all(|f| f.available && f.gaps.is_empty())
    );
}

#[test]
fn native_v9_tagged_templates_keep_measurable_calls_and_substitutions() {
    let source = "function run() { tag`text ${read(value)}`; obj.tag`text ${other}`; tag`outer ${nested`inner ${value}`}`; (choose ? first : second)`text ${last}`; }";
    let artifact = native_v9_fixture(&[("a.js", source)]);
    assert_eq!(artifact.coverage[0].state, "complete");
    assert_eq!(
        artifact.calls.len(),
        6,
        "all tags plus substitution read are invocations"
    );
    assert_eq!(
        artifact
            .calls
            .iter()
            .filter(|c| c.spelling.as_deref() == Some("tag"))
            .count(),
        3
    );
    for name in ["read", "nested"] {
        assert!(
            artifact
                .calls
                .iter()
                .any(|c| c.spelling.as_deref() == Some(name) && c.callee_range.is_some()),
            "{name}"
        );
    }
    let compound = artifact
        .calls
        .iter()
        .find(|c| c.spelling.as_deref() == Some("(choose ? first : second)"))
        .unwrap();
    assert!(compound.callee_range.is_none());
    for name in ["value", "other", "choose", "first", "second", "last"] {
        assert!(
            artifact.references.iter().any(|r| r.spelling == name),
            "{name}"
        );
    }
    assert!(
        artifact.capability_reports[0]
            .families
            .iter()
            .all(|f| f.available && f.gaps.is_empty())
    );
}

#[test]
fn native_v9_rust_scoped_and_generic_callees_use_only_final_identifier() {
    let artifact = native_v9_fixture(&[(
        "a.rs",
        "fn run() { module::target(); Type::target::<Item>(); value.method::<Item>(); (choose())(); }",
    )]);
    assert_eq!(
        artifact
            .calls
            .iter()
            .filter(|c| c.spelling.as_deref() == Some("target") && c.callee_range.is_some())
            .count(),
        2
    );
    assert!(
        artifact
            .calls
            .iter()
            .any(|c| c.spelling.as_deref() == Some("method") && c.callee_range.is_some())
    );
    assert!(
        artifact
            .calls
            .iter()
            .any(|c| c.spelling.as_deref() == Some("(choose())") && c.callee_range.is_none())
    );
}

#[test]
fn native_v9_java_varargs_share_signature_header_and_parameter_names() {
    let source = "class A { void f(String a, int... rest) {} void f(String a, String... rest) {} A(String a, long... rest) {} }";
    let artifact = native_v9_fixture(&[("A.java", source)]);
    let callables: Vec<_> = artifact
        .declarations
        .iter()
        .filter(|d| matches!(d.kind.as_str(), "method" | "constructor"))
        .collect();
    assert_eq!(callables.len(), 3);
    for (callable, (kind, variadic_type)) in callables.iter().zip([
        ("method", "int"),
        ("method", "String"),
        ("constructor", "long"),
    ]) {
        assert_eq!(callable.kind, kind);
        let signature = callable.key.signature.as_ref().unwrap();
        assert_eq!(signature.parameter_types, ["String", variadic_type]);
        assert!(signature.variadic);
        assert_eq!(callable.header.parameters.len(), 2);
        let rest = &callable.header.parameters[1];
        assert_eq!(rest.name.as_deref(), Some("rest"));
        assert_eq!(rest.type_name.as_deref(), Some(variadic_type));
        assert!(rest.variadic);
        assert_eq!(
            callable.key.ordinal, 0,
            "distinct type signatures are not sibling collisions"
        );
        for name in ["a", "rest"] {
            let declarations: Vec<_> = artifact
                .declarations
                .iter()
                .filter(|d| {
                    d.name.as_deref() == Some(name)
                        && callable.range.start <= d.range.start
                        && d.range.end <= callable.range.end
                })
                .collect();
            assert_eq!(
                declarations.len(),
                1,
                "{kind} {variadic_type}: one declaration for {name}"
            );
            let parameter = declarations[0];
            assert_eq!(parameter.kind, "parameter");
            assert_eq!(parameter.ancestors.last(), Some(&callable.key));
            let range = parameter.name_range.as_ref().unwrap();
            assert_eq!(&source[range.start..range.end], name);
            let references: Vec<_> = artifact
                .references
                .iter()
                .filter(|r| r.document == parameter.document && &r.range == range)
                .collect();
            assert_eq!(
                references.len(),
                1,
                "one lexical reference at the declarationName anchor"
            );
            assert_eq!(references[0].owner_syntax_id, callable.syntax_id);
            assert_eq!(references[0].spelling, name);
        }
        let rest_parameter = artifact
            .declarations
            .iter()
            .find(|d| {
                d.kind == "parameter"
                    && d.name.as_deref() == Some("rest")
                    && callable.range.start <= d.range.start
                    && d.range.end <= callable.range.end
            })
            .unwrap();
        assert_eq!(
            &source[rest_parameter.range.start..rest_parameter.range.end],
            format!("{variadic_type}... rest")
        );
    }
    assert_ne!(callables[0].syntax_id, callables[1].syntax_id);
    assert_eq!(
        artifact
            .declarations
            .iter()
            .filter(|d| d.name.as_deref() == Some("rest"))
            .count(),
        3
    );
}

#[test]
fn native_v9_rust_underscore_import_does_not_create_alias_declaration() {
    let artifact = native_v9_fixture(&[("a.rs", "use p::Original as _; use p::{Other as _};")]);
    assert!(!artifact.declarations.iter().any(|d| d.kind == "alias"));
    for imported in ["Original", "Other"] {
        assert!(artifact.references.iter().any(|r| r.spelling == imported));
    }
}
