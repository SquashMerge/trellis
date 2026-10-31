use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

// Independent serializer: this serializer deliberately does not call production native_ids.
fn encode(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => b"null".to_vec(),
        Value::Bool(true) => b"true".to_vec(),
        Value::Bool(false) => b"false".to_vec(),
        Value::Number(n) => n.to_string().into_bytes(),
        Value::String(s) => {
            let mut out = vec![b'"'];
            for c in s.chars() {
                match c {
                    '"' => out.extend(b"\\\""),
                    '\\' => out.extend(b"\\\\"),
                    '\u{0}'..='\u{1f}' => out.extend(format!("\\u{:04x}", c as u32).bytes()),
                    _ => {
                        let mut b = [0; 4];
                        out.extend(c.encode_utf8(&mut b).bytes());
                    }
                }
            }
            out.push(b'"');
            out
        }
        Value::Array(items) => {
            let mut out = vec![b'['];
            for (i, item) in items.iter().enumerate() {
                if i != 0 {
                    out.push(b',');
                }
                out.extend(encode(item));
            }
            out.push(b']');
            out
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let mut out = vec![b'{'];
            for (i, (key, value)) in entries.iter().enumerate() {
                if i != 0 {
                    out.push(b',');
                }
                out.extend(encode(&Value::String((*key).clone())));
                out.push(b':');
                out.extend(encode(value));
            }
            out.push(b'}');
            out
        }
    }
}

#[test]
fn every_normative_stable_id_vector() {
    let root: Value = serde_json::from_str(include_str!(
        "../../docs/semantic-evidence/id-test-vectors/stable-ids.json"
    ))
    .unwrap();
    let mut languages = BTreeMap::new();
    let mut production_ids = trellis::native_ids::IdentityRegistry::default();
    let cases = root["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 64);
    for case in cases {
        let language = case["language"].as_str().unwrap();
        *languages.entry(language).or_insert(0usize) += 1;
        let descriptor = &case["descriptor"];
        let input = json!({
            "sourceSet":descriptor["sourceSet"], "path":descriptor["path"],
            "language":descriptor["language"], "ancestors":descriptor["ancestors"],
            "declaration":descriptor["declaration"]
        });
        let bytes = encode(&input);
        let syntax = case["digests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|digest| digest["label"] == "syntax")
            .unwrap();
        assert_eq!(
            hex::encode(&bytes),
            syntax["inputHex"].as_str().unwrap(),
            "{} canonical bytes",
            case["caseId"]
        );
        let full = Sha256::digest([b"trellis.syntax.v1\0".as_slice(), bytes.as_slice()].concat());
        let expected = format!("sid:v1:{}", hex::encode(&full[..16]));
        assert_eq!(
            expected,
            case["expected"]["stableId"].as_str().unwrap(),
            "{} literal stable ID",
            case["caseId"]
        );
        assert_eq!(
            hex::encode(full),
            syntax["sha256"].as_str().unwrap(),
            "{} full digest",
            case["caseId"]
        );
        assert_eq!(
            production_ids.stable(&input).unwrap(),
            case["expected"]["stableId"].as_str().unwrap(),
            "{} production stable ID",
            case["caseId"]
        );
    }
    assert_eq!(
        languages,
        BTreeMap::from([
            ("java", 16),
            ("rust", 16),
            ("python", 16),
            ("javascript", 16)
        ])
    );
}

/// Backticked cells of each table row in the Decision 0003 `occ:v2` vector section.
type ContextRow = (String, String);
type OccurrenceRow = (String, String, String);
fn occurrence_v2_rows() -> (Vec<ContextRow>, Vec<OccurrenceRow>) {
    let doc = include_str!("../../docs/semantic-evidence/publication-rejoin-vectors-v1.md");
    let section = &doc[doc
        .find(r#"<a id="occurrence-identity-v2-decision-0003"></a>"#)
        .unwrap()..];
    let section = &section[..section.find("\n## ").unwrap_or(section.len())];
    let (mut contexts, mut occurrences) = (vec![], vec![]);
    for line in section.lines().filter(|l| l.starts_with("| ")) {
        // Labels may hold backticked text too; the vector cells start at the canonical input.
        let ticks: Vec<&str> = line
            .split('`')
            .skip(1)
            .step_by(2)
            .skip_while(|t| !t.starts_with('{'))
            .collect();
        match ticks.as_slice() {
            [input, full] if input.starts_with("{\"components\"") => {
                contexts.push((input.to_string(), full.to_string()))
            }
            [input, full, id] if input.starts_with("{\"contentHash\"") => {
                occurrences.push((input.to_string(), full.to_string(), id.to_string()))
            }
            _ => {}
        }
    }
    (contexts, occurrences)
}

#[test]
fn every_decision_0003_extraction_context_and_occurrence_vector() {
    let (contexts, occurrences) = occurrence_v2_rows();
    assert_eq!((contexts.len(), occurrences.len()), (2, 4));
    // The authenticated config capture and the hypothetical control capture.
    let config = [
        hex::encode(Sha256::digest(b"config-v1")),
        hex::encode(Sha256::digest(b"config-v2")),
    ];
    assert_eq!(
        config[0],
        "e3155b20e134632816c8611c4e9ee5cbd0e00689f7c4c955ee9f896580d02fdb"
    );
    for ((text, full), capture) in contexts.iter().zip(&config) {
        let input: Value = serde_json::from_str(text).unwrap();
        assert_eq!(input["components"][0]["hash"], json!(capture));
        let bytes = encode(&input);
        assert_eq!(bytes, text.as_bytes(), "canonical context bytes");
        assert_eq!(trellis::native_ids::canonical(&input), bytes);
        let digest = Sha256::digest(
            [
                b"trellis.extraction-context.v1\0".as_slice(),
                bytes.as_slice(),
            ]
            .concat(),
        );
        assert_eq!(&hex::encode(digest), full);
        let components = vec![("config".to_owned(), capture.clone())];
        assert_eq!(
            &trellis::native_ids::extraction_context("javascript", &components).unwrap(),
            full
        );
    }
    let mut production = trellis::native_ids::IdentityRegistry::default();
    let mut ids = vec![];
    for (text, full, id) in &occurrences {
        let input: Value = serde_json::from_str(text).unwrap();
        assert!(
            contexts
                .iter()
                .any(|(_, c)| input["extractionContext"] == json!(c))
        );
        let bytes = encode(&input);
        assert_eq!(bytes, text.as_bytes(), "canonical occurrence bytes");
        assert_eq!(trellis::native_ids::canonical(&input), bytes);
        let digest = hex::encode(Sha256::digest(
            [b"trellis.occurrence.v2\0".as_slice(), bytes.as_slice()].concat(),
        ));
        assert_eq!(&digest, full);
        assert_eq!(id, &format!("occ:v2:{}", &digest[..32]));
        assert_eq!(&production.occurrence(&input).unwrap(), id);
        ids.push(id.clone());
        // The withdrawn revision-bound v1 form of the same owner/kind/ordinal is refused by
        // production, and its v1 digest never equals any v2 digest or ID.
        for revision in ["r1", "r2"] {
            let v1 = json!({"revisionId":revision,"ownerSyntaxId":input["ownerSyntaxId"],
                "kind":input["kind"],"ordinal":input["ordinal"]});
            assert!(production.occurrence(&v1).is_err());
            let v1_digest = hex::encode(Sha256::digest(
                [
                    b"trellis.occurrence.v1\0".as_slice(),
                    encode(&v1).as_slice(),
                ]
                .concat(),
            ));
            assert!(
                occurrences
                    .iter()
                    .all(|(_, f, v2)| f != &v1_digest && v2[7..] != v1_digest[..32])
            );
        }
    }
    // Equal-ID rows differ only by kind; both controls change the call ID.
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 4);
    // The decision's reproduction check of the withdrawn v1 r1/call digest.
    let v1_call = json!({"revisionId":"r1","ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893","kind":"call","ordinal":0});
    let v1_digest = hex::encode(Sha256::digest(
        [
            b"trellis.occurrence.v1\0".as_slice(),
            encode(&v1_call).as_slice(),
        ]
        .concat(),
    ));
    assert!(v1_digest.starts_with("959c5606"), "{v1_digest}");
}

#[test]
fn native_v4_occurrence_changes_only_descriptor_input() {
    let content_hash = hex::encode(Sha256::digest(b"f();"));
    let context = trellis::native_ids::extraction_context("javascript", &[]).unwrap();
    let input = |version| {
        json!({"contentHash":content_hash,"extractionContext":context,
            "nativeProducerId":"trellis.native.syntax","nativeProducerVersion":version,
            "ownerSyntaxId":"sid:v1:7fd250597c82d08fcb73cabd62e89893",
            "kind":"call","ordinal":0})
    };
    let v3 = input("native-v3");
    let v4 = input("native-v4");
    let mut registry = trellis::native_ids::IdentityRegistry::default();
    let old = registry.occurrence(&v3).unwrap();
    let current = registry.occurrence(&v4).unwrap();
    assert_ne!(old, current);
    let canonical = encode(&v4);
    assert_eq!(canonical, trellis::native_ids::canonical(&v4));
    let digest =
        Sha256::digest([b"trellis.occurrence.v2\0".as_slice(), canonical.as_slice()].concat());
    assert_eq!(current, format!("occ:v2:{}", &hex::encode(digest)[..32]));
}
