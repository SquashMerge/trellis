//! Discovery properties for the public streaming boundary. Wire construction and the
//! coordinate model deliberately do not use the generated SCIP protobuf types.
use proptest::prelude::*;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    scip_raw::{
        self, AuthenticatedArtifact, CapturedClaim, CargoTargets, DocumentCap, DocumentSource,
        ModuleRoots, ScipCaptureError, VecSink,
    },
    semantic_batch::{BatchCounters, BatchLimits},
};

fn varint(mut n: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while n >= 128 {
        out.push((n as u8) | 128);
        n >>= 7;
    }
    out.push(n as u8);
    out
}
fn scalar(n: u64, v: u64) -> Vec<u8> {
    [varint(n << 3), varint(v)].concat()
}
fn bytes(n: u64, b: &[u8]) -> Vec<u8> {
    [varint((n << 3) | 2), varint(b.len() as u64), b.to_vec()].concat()
}
fn occurrence(range: Vec<u8>, symbol: &str, role: u64) -> Vec<u8> {
    [range, bytes(2, symbol.as_bytes()), scalar(3, role)].concat()
}
fn legacy(values: &[u64], mode: u8) -> Vec<u8> {
    match mode {
        0 => bytes(
            1,
            &values.iter().flat_map(|v| varint(*v)).collect::<Vec<_>>(),
        ),
        1 => values.iter().flat_map(|v| scalar(1, *v)).collect(),
        _ => values
            .chunks(2)
            .flat_map(|c| bytes(1, &c.iter().flat_map(|v| varint(*v)).collect::<Vec<_>>()))
            .collect(),
    }
}
fn typed(field: u64, values: &[u64]) -> Vec<u8> {
    bytes(
        field,
        &values
            .iter()
            .enumerate()
            .flat_map(|(i, v)| scalar(i as u64 + 1, *v))
            .collect::<Vec<_>>(),
    )
}
fn document(occurrences: &[Vec<u8>], extra: &[u8]) -> Vec<u8> {
    let mut d = [bytes(1, b"src/a.js"), bytes(4, b"javascript")].concat();
    for o in occurrences {
        d.extend(bytes(2, o));
    }
    d.extend_from_slice(extra);
    bytes(2, &d)
}
fn decode(
    wire: &[u8],
    source: &str,
    enc: u8,
    limits: &BatchLimits,
) -> Result<(Vec<CapturedClaim>, bool, BatchCounters), ScipCaptureError> {
    let roots = ModuleRoots::default();
    let targets = CargoTargets::default();
    let artifact = AuthenticatedArtifact {
        artifact_hash: "hash",
        producer_id: "producer",
        producer_version: "1",
        source_set_id: "set",
        source_revision: "revision",
        position_encoding: enc,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "",
        module_roots: &roots,
        cargo_targets: &targets,
        assigned_documents: 1,
    };
    let mut resolve = |_: &str, _: &str| {
        Ok(DocumentSource {
            language: "javascript".into(),
            bytes: Cow::Borrowed(source.as_bytes()),
            content_hash: "hash".into(),
        })
    };
    let mut counters = BatchCounters::default();
    let mut sink = VecSink::default();
    scip_raw::decode_streaming(
        wire,
        wire.len() as u64,
        &artifact,
        &mut resolve,
        &mut counters,
        limits,
        &mut sink,
        &Arc::new(AtomicBool::new(false)),
    )?;
    let capped = !sink.capped_documents().is_empty();
    Ok((sink.finish()?, capped, counters))
}
fn one(range: Vec<u8>, source: &str, enc: u8) -> CapturedClaim {
    decode(
        &document(&[occurrence(range, "s", 0)], &[]),
        source,
        enc,
        &BatchLimits::default(),
    )
    .unwrap()
    .0
    .remove(0)
}
// Build an independent coordinate/byte table by enumerating character boundaries of
// each line, counting UTF-8 bytes, UTF-16 units or Unicode scalars in each prefix.
fn boundaries(line: &str, enc: u8) -> Vec<(u64, usize)> {
    line.char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(line.len()))
        .map(|i| {
            let p = &line[..i];
            (
                match enc {
                    1 => p.len(),
                    2 => p.encode_utf16().count(),
                    _ => p.chars().count(),
                } as u64,
                i,
            )
        })
        .collect()
}
fn endpoints(v: &Value) -> ((u64, u64), (u64, u64)) {
    let n = |key: &str| v[key].as_u64().unwrap();
    match v["encoding"].as_str().unwrap() {
        "legacy" => {
            let a = v["values"].as_array().unwrap();
            let x: Vec<_> = a.iter().map(|v| v.as_u64().unwrap()).collect();
            if x.len() == 3 {
                ((x[0], x[1]), (x[0], x[2]))
            } else {
                ((x[0], x[1]), (x[2], x[3]))
            }
        }
        "singleLine" => (
            (n("line"), n("startCharacter")),
            (n("line"), n("endCharacter")),
        ),
        _ => (
            (n("startLine"), n("startCharacter")),
            (n("endLine"), n("endCharacter")),
        ),
    }
}

fn cases() -> u32 {
    std::env::var("TRELLIS_PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn ranges_and_wire_equivalence(a in prop::collection::vec(prop_oneof![Just('a'),Just('é'),Just('😀'),Just('\u{301}')], 1..7), b in prop::collection::vec(prop_oneof![Just('b'),Just('界'),Just('😀')], 1..7), enc in 1u8..=3, i in 0usize..20, j in 0usize..20, crlf in any::<bool>(), same in any::<bool>()) {
        let a: String = a.into_iter().collect(); let b: String = b.into_iter().collect();
        let sep = if crlf { "\r\n" } else { "\n" }; let source = format!("{a}{sep}{b}");
        let aa = boundaries(&a, enc); let bb = boundaries(if same {&a} else {&b}, enc);
        let (start, end) = if same { let x=i%aa.len(); let y=j%aa.len(); (aa[x.min(y)], aa[x.max(y)]) } else { (aa[i%aa.len()],bb[j%bb.len()]) };
        let el = u64::from(!same); let expected = ((0,start.0),(el,end.0));
        // Independently calculated byte endpoints also ensure the chosen range never
        // splits a Unicode scalar or CRLF sequence.
        let end_byte = if same {end.1} else {a.len()+sep.len()+end.1};
        prop_assert!(source.is_char_boundary(start.1) && source.is_char_boundary(end_byte));
        let four = [0,start.0,el,end.0];
        if (0, start.0) == (el, end.0) {
            // An empty non-marker range excludes its document with no claims (#652).
            let (claims, capped, _) = decode(&document(&[occurrence(legacy(&four,0), "s", 0)], &[]), &source, enc, &BatchLimits::default()).unwrap();
            prop_assert!(claims.is_empty() && capped);
            return Ok(());
        }
        let l4 = one(legacy(&four,0), &source, enc);
        prop_assert_eq!(endpoints(&l4.envelope.raw["range"]), expected);
        for mode in 1..=2 { let alt=one(legacy(&four,mode), &source, enc); prop_assert_eq!(&alt.canonical_bytes,&l4.canonical_bytes); prop_assert_eq!(&alt.fact_digest,&l4.fact_digest); }
        let typed4=one(typed(9,&four),&source,enc);
        prop_assert_eq!(endpoints(&typed4.envelope.raw["range"]),expected);
        prop_assert_ne!(&typed4.fact_digest,&l4.fact_digest);
        if same {
            let three=[0,start.0,end.0]; let l3=one(legacy(&three,0),&source,enc); let t3=one(typed(8,&three),&source,enc);
            prop_assert_eq!(endpoints(&l3.envelope.raw["range"]),expected); prop_assert_eq!(endpoints(&t3.envelope.raw["range"]),expected);
            prop_assert_ne!(&l3.fact_digest,&l4.fact_digest); prop_assert_ne!(&l3.fact_digest,&t3.fact_digest);
            for mode in 1..=2 { prop_assert_eq!(&one(legacy(&three,mode),&source,enc).fact_digest,&l3.fact_digest); }
        }
    }
    #[test]
    fn cap_retains_exact_definitions(roles in prop::collection::vec(any::<bool>(),0..9), cap in 0u64..8) {
        let os: Vec<_> = roles.iter().enumerate().map(|(i,r)| occurrence(legacy(&[0,0,1],0), &format!("s{i}"), u64::from(*r))).collect();
        let limits=BatchLimits { document_occurrences: cap, ..BatchLimits::default() };
        let symbol = [bytes(1,b"owner"),bytes(4,&bytes(1,b"target"))].concat();
        let (claims,capped,counters)=decode(&document(&os,&bytes(3,&symbol)),"x",1,&limits).unwrap();
        let over=roles.len() as u64>cap; let defs=roles.iter().filter(|r| **r).count() as u64;
        let expected: BTreeSet<_> = roles.iter().enumerate().filter(|(_,r)| !over || (defs<=cap && **r)).map(|(i,_)| format!("s{i}")).collect();
        let actual: BTreeSet<_> = claims.iter().filter(|c| c.envelope.record_kind == "occurrence").map(|c| c.envelope.raw["symbol"].as_str().unwrap().to_owned()).collect();
        prop_assert_eq!(actual,expected); prop_assert_eq!(capped,over); prop_assert_eq!(counters.candidates,roles.len() as u64 + 2);
        prop_assert_eq!(claims.iter().filter(|c| c.envelope.record_kind == "symbolInformation").count(),1);
        prop_assert_eq!(claims.iter().filter(|c| c.envelope.record_kind == "relationship").count(),1);
    }
}

#[test]
fn cross_line_typed_multiline_retains_end_line() {
    let c = one(typed(9, &[0, 0, 1, 4]), "éabc\r\n😀", 1);
    assert_eq!(
        c.envelope.raw["range"],
        json!({"encoding":"multiLine","startLine":0,"startCharacter":0,"endLine":1,"endCharacter":4})
    );
}

#[test]
fn enum_validation_including_unread_and_discarded_occurrences() {
    let sources = [
        "x",
        "// Trellis did not read this file: it is larger than --max-file-bytes.\n\u{1}\n",
        "// Trellis did not read this file: it is not UTF-8.\n\u{1}\n",
    ];
    // Document position; occurrence syntax; diagnostic severity/tag; symbol kind;
    // signature occurrence syntax. Zero is a known value in each pinned enum.
    for source in sources {
        for which in 0..6 {
            for bad in [false, true] {
                let value = if bad { 999 } else { 0 };
                let mut o = occurrence(legacy(&[0, 0, 1], 0), "s", 0);
                let mut extra = Vec::new();
                match which {
                    0 => extra.extend(scalar(6, value)),
                    1 => o.extend(scalar(5, value)),
                    2 => o.extend(bytes(6, &scalar(1, value))),
                    3 => o.extend(bytes(6, &scalar(5, value))),
                    4 => extra.extend(bytes(3, &scalar(5, value))),
                    _ => {
                        let sig_occ =
                            [occurrence(legacy(&[0, 0, 1], 0), "s", 0), scalar(5, value)].concat();
                        let sig = [bytes(5, b"x"), bytes(2, &sig_occ)].concat();
                        extra.extend(bytes(3, &bytes(7, &sig)));
                    }
                }
                let result = decode(&document(&[o], &extra), source, 1, &BatchLimits::default());
                if bad {
                    assert!(
                        matches!(result, Err(ScipCaptureError::InvalidArtifact(_))),
                        "case {which} source {source:?}: {result:?}"
                    );
                } else {
                    let (claims, _, _) = result.unwrap();
                    if source != "x" {
                        assert!(claims.is_empty());
                    }
                }
            }
        }
        let good = occurrence(legacy(&[0, 0, 1], 0), "first", 1);
        let bad = [
            occurrence(legacy(&[0, 0, 1], 0), "discarded", 0),
            scalar(5, 999),
        ]
        .concat();
        let limits = BatchLimits {
            document_occurrences: 1,
            ..BatchLimits::default()
        };
        assert!(matches!(
            decode(&document(&[good, bad], &[]), source, 1, &limits),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn generated_unknown_enums_are_rejected(value in 1000u64..100000, which in 0usize..6, source_kind in 0usize..3, capped in any::<bool>()) {
        let sources = ["x", "// Trellis did not read this file: it is larger than --max-file-bytes.\n\u{1}\n", "// Trellis did not read this file: it is not UTF-8.\n\u{1}\n"];
        let mut o = occurrence(legacy(&[0,0,1],0),"s",0);
        let mut extra = Vec::new();
        match which {
            0 => extra.extend(scalar(6,value)),
            1 => o.extend(scalar(5,value)),
            2 => o.extend(bytes(6,&scalar(1,value))),
            3 => o.extend(bytes(6,&bytes(5,&varint(value)))),
            4 => extra.extend(bytes(3,&scalar(5,value))),
            _ => { let sig_occ=[occurrence(legacy(&[0,0,1],0),"s",0),scalar(5,value)].concat(); let sig=[bytes(5,b"x"),bytes(2,&sig_occ)].concat(); extra.extend(bytes(3,&bytes(7,&sig))); }
        }
        let mut limits=BatchLimits::default();
        let os=if capped { limits.document_occurrences=1; vec![occurrence(legacy(&[0,0,1],0),"first",1),o] } else { vec![o] };
        let result=decode(&document(&os,&extra),sources[source_kind],1,&limits);
        prop_assert!(matches!(result,Err(ScipCaptureError::InvalidArtifact(_))), "which={which}, value={value}, source={source_kind}, capped={capped}: {result:?}");
    }
    #[test]
    fn all_generated_column_boundaries_match_model(chars in prop::collection::vec(prop_oneof![Just('a'),Just('é'),Just('😀'),Just('\u{301}')],1..8), enc in 1u8..=3) {
        let source: String=chars.into_iter().collect();
        let table=boundaries(&source,enc);
        let max=table.last().unwrap().0;
        for column in 0..=max+1 {
            let result=decode(&document(&[occurrence(legacy(&[0,0,column],0),"s",0)],&[]),&source,enc,&BatchLimits::default());
            let valid=table.iter().any(|(c,_)| *c==column);
            prop_assert_eq!(result.is_ok(),valid,"source={:?} enc={} column={}",source,enc,column);
        }
    }
    #[test]
    fn injected_limits_are_inclusive(n in 1usize..8) {
        let os: Vec<_>=(0..n).map(|i| occurrence(legacy(&[0,0,1],0),&format!("s{i}"),0)).collect();
        let wire=document(&os,&[]);
        // One top-level document + path + language, then for each occurrence:
        // occurrence field, range field, three packed elements, symbol, roles.
        let fields=3+7*n as u64;
        let l=BatchLimits { artifact_bytes: wire.len() as u64, candidates: n as u64, wire_fields: fields, ..BatchLimits::default() };
        let (_,_,c)=decode(&wire,"x",1,&l).unwrap();
        prop_assert_eq!(c.wire_fields,fields); prop_assert_eq!(c.candidates,n as u64);
        let mut short=l.clone(); short.artifact_bytes-=1;
        prop_assert!(matches!(decode(&wire,"x",1,&short),Err(ScipCaptureError::LimitExceeded(ref s)) if s=="artifact bytes"));
        short=l.clone(); short.candidates-=1;
        prop_assert!(matches!(decode(&wire,"x",1,&short),Err(ScipCaptureError::LimitExceeded(ref s)) if s=="candidate facts"));
        short=l; short.wire_fields-=1;
        prop_assert!(matches!(decode(&wire,"x",1,&short),Err(ScipCaptureError::LimitExceeded(_))));
        let reserve: u64=(0..n).map(|i| 512+6*("src/a.js".len()+"javascript".len()+format!("s{i}").len()) as u64).sum();
        let mut unit=BatchLimits { unit_expansion_bytes: reserve, ..BatchLimits::default() };
        prop_assert!(decode(&wire,"x",1,&unit).is_ok());
        unit.unit_expansion_bytes-=1;
        prop_assert!(matches!(decode(&wire,"x",1,&unit),Err(ScipCaptureError::LimitExceeded(ref s)) if s=="unit expansion"));
    }
}

#[test]
fn invalid_unicode_and_crlf_columns_are_rejected() {
    for (source, enc, invalid) in [("éx", 1, 1), ("😀x", 2, 1), ("x\r\ny", 1, 2)] {
        let wire = document(&[occurrence(legacy(&[0, 0, invalid], 0), "s", 0)], &[]);
        assert!(matches!(
            decode(&wire, source, enc, &BatchLimits::default()),
            Err(ScipCaptureError::InvalidArtifact(_))
        ));
    }
}

// #655: unlike the single-document coordinate property above, keep multiple
// independently named documents and compare to the artifact without the bad ones.
fn doc_655(path: &str, fields: &[Vec<u8>]) -> Vec<u8> {
    let mut body = [bytes(1, path.as_bytes()), bytes(4, b"javascript")].concat();
    for field in fields {
        body.extend(bytes(2, field));
    }
    bytes(2, &body)
}

fn decode_655(
    wire: &[u8],
    count: usize,
    enc: u8,
) -> Result<(Vec<CapturedClaim>, scip_raw::CappedDocuments), ScipCaptureError> {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(wire)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let roots = ModuleRoots::default();
    let targets = CargoTargets::default();
    let artifact = AuthenticatedArtifact {
        artifact_hash: &hash,
        producer_id: "test",
        producer_version: "1",
        source_set_id: "set",
        source_revision: "rev",
        position_encoding: enc,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "",
        module_roots: &roots,
        cargo_targets: &targets,
        assigned_documents: count as u64,
    };
    let mut sink = VecSink::default();
    scip_raw::decode_streaming(
        wire,
        wire.len() as u64,
        &artifact,
        &mut |_: &str, _: &str| {
            Ok(DocumentSource {
                language: "javascript".into(),
                bytes: Cow::Borrowed("aé😀xyz\nsecond".as_bytes()),
                content_hash: "source".into(),
            })
        },
        &mut BatchCounters::default(),
        &BatchLimits::PRODUCTION,
        &mut sink,
        &Arc::new(AtomicBool::new(false)),
    )?;
    let caps = sink.capped_documents().clone();
    Ok((sink.finish()?, caps))
}

fn range_655(values: &[u64], mode: u8, enclosing: bool) -> Vec<u8> {
    match (mode, enclosing) {
        (0, false) => legacy(values, 0),
        (0, true) => bytes(
            7,
            &values.iter().flat_map(|v| varint(*v)).collect::<Vec<_>>(),
        ),
        (_, false) => typed(if values.len() == 3 { 8 } else { 9 }, values),
        (_, true) => typed(if values.len() == 3 { 10 } else { 11 }, values),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn invalid_range_655_multi_document_isolation(enc in 1u8..=3, mode in 0u8..=1, spec in prop::collection::vec((0u8..=3, any::<bool>()), 2..7), rotate in 0usize..10) {
        // Three classes of bad document are always present; generated extras and
        // rotation vary multiplicity, placement, wire shape and range positions.
        let mut specs = vec![(0,false), (1,true), (2,false), (3,true)];
        specs.extend(spec);
        let len = specs.len(); specs.rotate_left(rotate % len);
        let mut base = Vec::new(); let mut full = Vec::new(); let mut expected_caps = std::collections::BTreeMap::new();
        for (i, (kind, multiline)) in specs.into_iter().enumerate() {
            let path = format!("src/{i}.js");
            let healthy = occurrence(range_655(&[0,0,1], mode, false), &format!("s{i}"), 1);
            let mut fields = vec![healthy];
            if kind != 3 {
                let reversed = if multiline { vec![1,1,0,1] } else { vec![0,1,0] };
                let empty = if multiline { vec![0,1,0,1] } else { vec![0,1,1] };
                let bad = match kind {
                    0 => occurrence(range_655(&reversed, mode, false), "bad", 0),
                    1 => occurrence(range_655(&empty, mode, false), "bad", 0),
                    _ => [occurrence(range_655(&[0,0,1], mode, false), "bad", 0), range_655(&reversed, mode, true)].concat(),
                };
                fields.push(bad);
                expected_caps.insert(("javascript".to_string(), path.clone()), DocumentCap::InvalidRange);
            } else { base.extend(doc_655(&path, &fields)); }
            full.extend(doc_655(&path, &fields));
        }
        let (expected, base_caps) = decode_655(&base, len - expected_caps.len(), enc).unwrap();
        let (actual, caps) = decode_655(&full, len, enc).unwrap();
        prop_assert!(!expected.is_empty()); prop_assert!(base_caps.is_empty());
        prop_assert_eq!(&caps, &expected_caps);
        prop_assert!(actual.iter().all(|c| !caps.contains_key(&(c.language.clone(), c.envelope.relative_path.clone()))));
        let project = |claims: &[CapturedClaim]| claims.iter().map(|c| (c.canonical_bytes.clone(), c.fact_digest.clone())).collect::<Vec<_>>();
        prop_assert_eq!(project(&actual), project(&expected));
    }
}

#[test]
fn empty_enclosing_655_control() {
    let primary = occurrence(legacy(&[0, 0, 1], 0), "s", 0);
    let wire = doc_655(
        "src/a.js",
        &[[primary, range_655(&[0, 1, 1], 0, true)].concat()],
    );
    let (claims, caps) = decode_655(&wire, 1, 1).unwrap();
    assert_eq!(claims.len(), 1);
    assert!(caps.is_empty());
    eprintln!("empty enclosing: admitted with one claim and no cap");
}

// Owner-requested behavior, intentionally retained as an ignored red witness:
// #652's ratified paragraph explicitly names empty primary, not empty enclosing.
#[test]
#[ignore = "requested empty-enclosing exclusion differs from the ratified paragraph"]
fn empty_enclosing_655_requested_exclusion() {
    let wire = doc_655(
        "src/a.js",
        &[[
            occurrence(legacy(&[0, 0, 1], 0), "s", 0),
            range_655(&[0, 1, 1], 0, true),
        ]
        .concat()],
    );
    let (claims, caps) = decode_655(&wire, 1, 1).unwrap();
    assert_eq!(
        caps.get(&("javascript".into(), "src/a.js".into())),
        Some(&DocumentCap::InvalidRange),
        "owner-requested exclusion; claims={}",
        claims.len()
    );
    assert!(claims.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), ..ProptestConfig::default() })]
    #[test]
    fn invalid_range_655_other_malformations_reject(kind in 0u8..4, before in any::<bool>(), same in any::<bool>(), enc in 1u8..=3, mode in 0u8..=1) {
        let healthy = doc_655("src/a.js", &[occurrence(range_655(&[0,0,1], mode, false), "good", 1)]);
        let invalid = occurrence(range_655(&[0,1,0], mode, false), "bad", 0);
        let malformed = match kind {
            0 => occurrence(range_655(&[9,0,1], mode, false), "outside", 0),
            1 => occurrence(range_655(&[0,0,1], mode, false), "roles", 128),
            // UTF-8 column 2 splits é; UTF-16 column 3 splits 😀. UTF-32
            // has no split scalar, so use an unknown enum there.
            2 if enc == 1 => occurrence(range_655(&[0,0,2], mode, false), "split", 0),
            2 if enc == 2 => occurrence(range_655(&[0,0,3], mode, false), "split", 0),
            2 => [occurrence(range_655(&[0,0,1], mode, false), "enum", 0), scalar(5,999)].concat(),
            _ => vec![0x0a, 0x02, 0x80], // truncated packed range inside occurrence
        };
        let (b,c) = if same {
            let fields = if before { vec![malformed, invalid] } else { vec![invalid, malformed] };
            (doc_655("src/b.js", &fields), Vec::new())
        } else {
            let b = doc_655("src/b.js", &[invalid]);
            let c = doc_655("src/c.js", &[malformed]);
            if before { (c,b) } else { (b,c) }
        };
        let wire = [healthy,b,c].concat();
        let result = decode_655(&wire, if same {2} else {3}, enc);
        prop_assert!(matches!(result, Err(ScipCaptureError::InvalidArtifact(_))), "{result:?}");
    }
}
