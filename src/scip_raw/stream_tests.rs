//! Equivalence of the streaming decoder with the whole-Index decoder it replaced, which is kept
//! here verbatim as the test-only oracle, and the one-live-unit and limit properties of streaming.
use super::tests::{
    BRANCHES, EXTERNAL, FULL, LEGACY, OMITTED, SIGNATURE, message, vector_documents, vector_input,
};
use super::*;
use std::sync::{Arc, atomic::AtomicBool};

// Conservative envelope expansion bound before any relationship JSON or canonical bytes exist.
// Strings may be copied into JSON and canonical output; fixed overhead covers keys/escaping.
const MAX_CLAIM_BYTES: usize = 128 * 1024 * 1024;
fn reserve_claim(total: &mut usize, strings: &[&str]) -> Result<(), ScipCaptureError> {
    let bytes = strings
        .iter()
        .try_fold(512usize, |size, text| {
            size.checked_add(text.len().checked_mul(6)?)
        })
        .ok_or_else(|| limit("claim expansion"))?;
    *total = total
        .checked_add(bytes)
        .filter(|v| *v <= MAX_CLAIM_BYTES)
        .ok_or_else(|| limit("claim expansion"))?;
    Ok(())
}
fn claim_budget(index: &types::Index, flag: &CancelFlag) -> Result<(), ScipCaptureError> {
    let mut total = 0usize;
    let mut traversed = 0usize;
    for d in &index.documents {
        for o in &d.occurrences {
            reserve_claim(&mut total, &[&d.relative_path, &d.language, &o.symbol])?;
            traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(flag)?;
            }
        }
        for s in &d.symbols {
            reserve_claim(
                &mut total,
                &[
                    &d.relative_path,
                    &d.language,
                    &s.symbol,
                    &s.display_name,
                    &s.enclosing_symbol,
                ],
            )?;
            for r in &s.relationships {
                reserve_claim(
                    &mut total,
                    &[&d.relative_path, &d.language, &s.symbol, &r.symbol],
                )?;
                traversed += 1;
                if traversed.is_multiple_of(1024) {
                    cancelled(flag)?;
                }
            }
        }
    }
    Ok(())
}
fn emit(
    claims: &mut Vec<CapturedClaim>,
    input: &AuthenticatedScipInput<'_>,
    d: &types::Document,
    language: &str,
    kind: &str,
    raw: Value,
) {
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
        module_roots: &super::ModuleRoots::default(),
        cargo_targets: &super::CargoTargets::default(),
        assigned_documents: 0,
    };
    claims.push(make_claim(&artifact, d, language, kind, raw));
}
pub(super) fn decode_whole_index_oracle(
    input: &AuthenticatedScipInput<'_>,
    flag: &CancelFlag,
) -> Result<CapturedScipInventory, ScipCaptureError> {
    cancelled(flag)?;
    if input.artifact_bytes.len() as u64 > BatchLimits::PRODUCTION.artifact_bytes {
        return Err(limit("artifact bytes"));
    }
    if sha(input.artifact_bytes) != input.artifact_hash {
        return Err(ScipCaptureError::InvalidAdmission("artifact digest".into()));
    }
    if input.decoder_version != DECODER_VERSION
        || !matches!(input.position_encoding, 1..=3)
        || input.protocol_version != 0
        || input.text_document_encoding != 1
        || input.profile_id.is_empty()
    {
        return Err(ScipCaptureError::InvalidAdmission("decoder context".into()));
    }
    cancelled(flag)?;
    wire(
        input.artifact_bytes,
        Node::Index,
        0,
        &mut WireBudget::default(),
        flag,
    )?;
    cancelled(flag)?;
    let index = types::Index::parse_from_bytes(input.artifact_bytes)
        .map_err(|_| invalid("protobuf parse"))?;
    unknown(&index.special_fields)?;
    if let Some(meta) = index.metadata.as_ref() {
        unknown(&meta.special_fields)?;
        if known(meta.version)? != input.protocol_version
            || known(meta.text_document_encoding)? != input.text_document_encoding
            || !meta.project_root.is_empty() && meta.project_root != input.project_root_uri
        {
            return Err(invalid("metadata binding"));
        }
        if let Some(tool) = meta.tool_info.as_ref() {
            unknown(&tool.special_fields)?;
            if !tool.name.is_empty() && tool.name != input.producer_id
                || !tool.version.is_empty() && tool.version != input.producer_version
            {
                return Err(invalid("tool binding"));
            }
        }
    }
    if index.documents.len() > 1000 || index.external_symbols.len() > 10000 {
        return Err(limit("index entries"));
    }
    claim_budget(&index, flag)?;
    let mut count = 0usize;
    let mut traversed = 0usize;
    let mut relationships = 0usize;
    for s in &index.external_symbols {
        traversed += 1;
        if traversed.is_multiple_of(1024) {
            cancelled(flag)?;
        }
        let _ = symbol(s, input.position_encoding, flag, &mut traversed, false)?;
        relationships += s.relationships.len();
    }
    let mut admitted = BTreeMap::new();
    for d in &input.documents {
        if d.content_hash != sha(d.source_bytes)
            || !valid_path(&d.relative_path)
            || admitted.insert(d.relative_path.as_str(), d).is_some()
        {
            return Err(ScipCaptureError::InvalidAdmission(
                "source document hash or duplicate".into(),
            ));
        }
    }
    let mut seen = BTreeSet::new();
    let mut claims = Vec::new();
    for d in &index.documents {
        cancelled(flag)?;
        unknown(&d.special_fields)?;
        if !seen.insert(d.relative_path.as_str()) {
            return Err(invalid("duplicate document path"));
        }
        let source = admitted
            .get(d.relative_path.as_str())
            .ok_or_else(|| invalid("unlisted document"))?;
        let language =
            bind_document_language(&d.language, &source.language, &input.profile_languages)?;
        if d.text.as_bytes() != source.source_bytes && !d.text.is_empty() {
            return Err(invalid("document source binding"));
        }
        let raw = known(d.position_encoding)?;
        if !(0..=3).contains(&raw) || raw != 0 && raw != i32::from(input.position_encoding) {
            return Err(invalid("position encoding"));
        }
        if d.occurrences.len() > 10000 || d.symbols.len() > 10000 {
            return Err(limit("document entries"));
        }
        let text =
            std::str::from_utf8(source.source_bytes).map_err(|_| invalid("non-UTF8 source"))?;
        let mut positions = Vec::with_capacity(d.occurrences.len() * 4);
        for o in &d.occurrences {
            count += 1;
            if count > 100000 {
                return Err(limit("candidate facts"));
            }
            if count.is_multiple_of(1024) {
                cancelled(flag)?;
            }
            emit(
                &mut claims,
                input,
                d,
                language,
                "occurrence",
                occurrence(o, &mut positions)?,
            );
        }
        validate_positions(text, input.position_encoding, &mut positions, flag)?;
        for s in &d.symbols {
            traversed += 1;
            if traversed.is_multiple_of(1024) {
                cancelled(flag)?;
            }
            let (info, rels) = symbol(s, input.position_encoding, flag, &mut traversed, true)?;
            count += 1;
            relationships += rels.len();
            if count + rels.len() > 100000 {
                return Err(limit("candidate facts"));
            }
            emit(&mut claims, input, d, language, "symbolInformation", info);
            for r in rels {
                count += 1;
                if count.is_multiple_of(1024) {
                    cancelled(flag)?;
                }
                emit(&mut claims, input, d, language, "relationship", r);
            }
        }
    }
    if relationships > 20000 {
        return Err(limit("relationships"));
    }
    if seen.len() != admitted.len() {
        return Err(invalid("missing artifact document"));
    }
    claims.sort_by(|a, b| {
        a.language
            .as_bytes()
            .cmp(b.language.as_bytes())
            .then(
                a.envelope
                    .relative_path
                    .as_bytes()
                    .cmp(b.envelope.relative_path.as_bytes()),
            )
            .then(a.canonical_bytes.cmp(&b.canonical_bytes))
    });
    claims.dedup_by(|a, b| a.language == b.language && a.canonical_bytes == b.canonical_bytes);
    let mut digests = BTreeMap::new();
    for claim in &claims {
        if let Some(bytes) = digests.insert(&claim.fact_digest, &claim.canonical_bytes)
            && bytes != &claim.canonical_bytes
        {
            return Err(ScipCaptureError::DigestCollision);
        }
    }
    Ok(CapturedScipInventory {
        artifact_hashes: vec![input.artifact_hash.clone()],
        decoder_version: input.decoder_version.into(),
        source_manifest_hash: input.source_manifest_hash.clone(),
        completion: input.completion,
        exclusions: input.exclusions.clone(),
        capped_documents: BTreeSet::new(),
        claims,
    })
}

const SOURCE: &[u8] = b"function f(){g();}\n";

fn flag() -> CancelFlag {
    Arc::new(AtomicBool::new(false))
}
fn streamed(input: &AuthenticatedScipInput<'_>) -> Result<CapturedScipInventory, ScipCaptureError> {
    decode_authenticated(input, &flag())
}
fn oracle(input: &AuthenticatedScipInput<'_>) -> Result<CapturedScipInventory, ScipCaptureError> {
    decode_whole_index_oracle(input, &flag())
}
fn assert_equivalent(name: &str, bytes: &[u8], docs: Vec<AuthenticatedScipDocument<'_>>) {
    let input = vector_input(bytes, docs);
    let expected = oracle(&input).unwrap_or_else(|e| panic!("{name}: oracle rejected {e:?}"));
    let actual = streamed(&input).unwrap_or_else(|e| panic!("{name}: stream rejected {e:?}"));
    assert!(!expected.claims.is_empty(), "{name}");
    assert_eq!(actual.claims, expected.claims, "{name}");
    assert_eq!(actual.artifact_hashes, expected.artifact_hashes, "{name}");
}
fn metadata_body() -> Vec<u8> {
    let tool = [
        message(1, b"scip-test"),
        message(2, b"1.0.0"),
        message(3, b"--a"),
        message(3, b"--b"),
    ]
    .concat();
    [
        vec![0x20, 1],
        message(3, b"file:///synthetic"),
        message(2, &tool),
    ]
    .concat()
}

#[test]
fn streaming_matches_whole_index_oracle_on_pinned_vectors() {
    for (name, wire_hex) in [
        ("FULL", FULL),
        ("EXTERNAL", EXTERNAL),
        ("SIGNATURE", SIGNATURE),
        ("OMITTED", OMITTED),
        ("LEGACY", LEGACY),
        ("BRANCHES", BRANCHES),
    ] {
        let bytes = hex::decode(wire_hex).unwrap();
        assert_equivalent(name, &bytes, vector_documents(wire_hex));
    }
}

#[test]
fn streaming_matches_oracle_with_metadata_after_documents() {
    let bytes = [hex::decode(FULL).unwrap(), message(1, &metadata_body())].concat();
    assert_equivalent("late metadata", &bytes, vector_documents(FULL));
}

#[test]
fn repeated_metadata_merges_like_parse_from_bytes() {
    let first = [
        message(1, b"scip-test"),
        message(3, b"--a"),
        message(3, b"--b"),
    ]
    .concat();
    let second = [
        vec![0x20, 1],
        message(3, b"file:///synthetic"),
        message(2, &[message(2, b"1.0.0"), message(3, b"--c")].concat()),
    ]
    .concat();
    let repeated = [
        message(1, &message(2, &first)),
        hex::decode(FULL).unwrap(),
        message(1, &second),
    ]
    .concat();
    let merged = [message(1, &metadata_body()), hex::decode(FULL).unwrap()].concat();
    let docs = vector_documents(FULL);
    let expected = oracle(&vector_input(&merged, vector_documents(FULL))).unwrap();
    let actual = streamed(&vector_input(&repeated, docs)).unwrap();
    let facts = |inventory: &CapturedScipInventory| {
        inventory
            .claims
            .iter()
            .map(|c| (c.canonical_bytes.clone(), c.fact_digest.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(facts(&actual), facts(&expected));
    assert!(
        matches!(
            oracle(&vector_input(&repeated, vector_documents(FULL))),
            Err(ScipCaptureError::LimitExceeded(reason)) if reason == "repeated field"
        ),
        "the whole-Index walk rejected repeated metadata; streaming merges it"
    );
}

#[test]
fn merged_metadata_is_validated_once_at_the_end() {
    let wrong_version = [
        message(1, &message(2, &message(2, b"wrong-version"))),
        hex::decode(FULL).unwrap(),
        message(1, &[0x20, 1]),
    ]
    .concat();
    assert!(matches!(
        streamed(&vector_input(&wrong_version, vector_documents(FULL))),
        Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "tool binding"
    ));
}

#[test]
fn streaming_matches_oracle_with_external_symbols_after_documents() {
    let full = hex::decode(FULL).unwrap();
    let doc_a_len = 2 + usize::from(full[1]);
    let external = message(3, &[message(1, b"external#"), vec![0x28, 17]].concat());
    let interleaved = [
        full[..doc_a_len].to_vec(),
        external.clone(),
        full[doc_a_len..].to_vec(),
        external,
    ]
    .concat();
    assert_equivalent("external symbols", &interleaved, vector_documents(FULL));
}

#[derive(Default)]
struct CountingSink {
    begun: usize,
    ended: usize,
    max_open: usize,
    claims: usize,
}
impl ClaimSink for CountingSink {
    fn begin_document(&mut self, _: &UnitDocument<'_>) -> Result<(), ScipCaptureError> {
        self.begun += 1;
        self.max_open = self.max_open.max(self.begun - self.ended);
        Ok(())
    }
    fn claim(&mut self, _: CapturedClaim) -> Result<(), ScipCaptureError> {
        self.claims += 1;
        Ok(())
    }
    fn end_document(&mut self) -> Result<(), ScipCaptureError> {
        self.ended += 1;
        Ok(())
    }
}

fn uniform_artifact(documents: usize) -> Vec<u8> {
    let occurrence = message(2, &[0x0a, 3, 0, 9, 10]);
    (0..documents)
        .flat_map(|n| {
            let doc = [
                message(1, format!("src/f{n:04}.js").as_bytes()),
                message(4, b"javascript"),
                occurrence.clone(),
                vec![0x30, 1],
            ]
            .concat();
            message(2, &doc)
        })
        .collect()
}
fn decode_uniform(
    documents: usize,
    limits: &BatchLimits,
    sink: &mut dyn ClaimSink,
) -> (Result<ArtifactSummary, ScipCaptureError>, BatchCounters) {
    let bytes = uniform_artifact(documents);
    let hash = sha(&bytes);
    let artifact = AuthenticatedArtifact {
        artifact_hash: &hash,
        producer_id: "scip-test",
        producer_version: "1.0.0",
        source_set_id: "app",
        source_revision: "r1",
        position_encoding: 1,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "file:///synthetic",
        module_roots: &super::ModuleRoots::default(),
        cargo_targets: &super::CargoTargets::default(),
        assigned_documents: documents as u64,
    };
    let mut resolve = |_: &str, _: &str| {
        Ok(DocumentSource {
            language: "javascript".into(),
            bytes: Cow::Borrowed(SOURCE),
            content_hash: sha(SOURCE),
        })
    };
    let mut counters = BatchCounters::default();
    let result = decode_streaming(
        bytes.as_slice(),
        bytes.len() as u64,
        &artifact,
        &mut resolve,
        &mut counters,
        limits,
        sink,
        &flag(),
    );
    (result, counters)
}

#[test]
fn decoder_holds_one_document_unit_at_a_time() {
    let mut observed = Vec::new();
    for documents in [10, 40] {
        let mut sink = CountingSink::default();
        let (summary, counters) = decode_uniform(documents, &BatchLimits::PRODUCTION, &mut sink);
        assert_eq!(summary.unwrap().documents, documents as u64);
        assert_eq!(sink.begun, documents);
        assert_eq!(sink.ended, documents);
        assert_eq!(sink.claims, documents);
        assert!(sink.max_open <= 1);
        assert_eq!(counters.live_unit_buffers_high_water, 1);
        assert_eq!(counters.live_unit_buffers, 0);
        observed.push((
            counters.unit_buffer_high_water_bytes,
            counters.unit_reservation_high_water_bytes,
        ));
    }
    assert_eq!(observed[0], observed[1]);
}

#[test]
fn unit_length_over_limit_is_rejected_before_allocation() {
    let limits = BatchLimits {
        unit_expansion_bytes: 16,
        ..BatchLimits::PRODUCTION
    };
    let (result, counters) = decode_uniform(2, &limits, &mut CountingSink::default());
    assert_eq!(result, Err(limit("unit expansion")));
    assert_eq!(counters.live_unit_buffers_high_water, 0);
    assert_eq!(counters.unit_buffer_high_water_bytes, 0);
}

#[test]
fn unit_length_beyond_remaining_bytes_is_a_parse_error_before_allocation() {
    let mut bytes = vec![0x12];
    bytes.extend(var_len(1 << 30));
    let hash = sha(&bytes);
    let artifact = AuthenticatedArtifact {
        artifact_hash: &hash,
        producer_id: "scip-test",
        producer_version: "1.0.0",
        source_set_id: "app",
        source_revision: "r1",
        position_encoding: 1,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "file:///synthetic",
        module_roots: &super::ModuleRoots::default(),
        cargo_targets: &super::CargoTargets::default(),
        assigned_documents: 0,
    };
    let mut counters = BatchCounters::default();
    let result = decode_streaming(
        bytes.as_slice(),
        bytes.len() as u64,
        &artifact,
        &mut |_: &str, _: &str| Err(invalid("unlisted document")),
        &mut counters,
        &BatchLimits::PRODUCTION,
        &mut CountingSink::default(),
        &flag(),
    );
    assert_eq!(result, Err(invalid("protobuf parse")));
    assert_eq!(counters.live_unit_buffers_high_water, 0);
}
fn var_len(x: usize) -> Vec<u8> {
    super::tests::var(x)
}

#[test]
fn batch_limits_are_named_and_enforced() {
    for (name, limits, documents) in [
        (
            "candidate facts",
            BatchLimits {
                candidates: 3,
                ..BatchLimits::PRODUCTION
            },
            4,
        ),
        (
            "wire fields",
            BatchLimits {
                wire_fields: 10,
                ..BatchLimits::PRODUCTION
            },
            4,
        ),
        (
            "artifact bytes",
            BatchLimits {
                artifact_bytes: 10,
                ..BatchLimits::PRODUCTION
            },
            1,
        ),
    ] {
        let (result, _) = decode_uniform(documents, &limits, &mut CountingSink::default());
        assert_eq!(result, Err(limit(name)), "{name}");
    }
}

#[test]
fn candidate_budget_is_shared_across_artifacts() {
    let limits = BatchLimits {
        candidates: 5,
        ..BatchLimits::PRODUCTION
    };
    let bytes = uniform_artifact(3);
    let hash = sha(&bytes);
    let artifact = AuthenticatedArtifact {
        artifact_hash: &hash,
        producer_id: "scip-test",
        producer_version: "1.0.0",
        source_set_id: "app",
        source_revision: "r1",
        position_encoding: 1,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "file:///synthetic",
        module_roots: &super::ModuleRoots::default(),
        cargo_targets: &super::CargoTargets::default(),
        assigned_documents: 3,
    };
    let mut counters = BatchCounters::default();
    let run = |counters: &mut BatchCounters| {
        decode_streaming(
            bytes.as_slice(),
            bytes.len() as u64,
            &artifact,
            &mut |_: &str, _: &str| {
                Ok(DocumentSource {
                    language: "javascript".into(),
                    bytes: Cow::Borrowed(SOURCE),
                    content_hash: sha(SOURCE),
                })
            },
            counters,
            &limits,
            &mut CountingSink::default(),
            &flag(),
        )
    };
    run(&mut counters).expect("3 candidates fit");
    assert_eq!(run(&mut counters), Err(limit("candidate facts")));
}

#[test]
fn external_symbols_over_limit_are_rejected() {
    let external = message(3, &message(1, b"external#"));
    let bytes = [external.clone(), external].concat();
    let limits = BatchLimits {
        external_symbols: 1,
        ..BatchLimits::PRODUCTION
    };
    let hash = sha(&bytes);
    let artifact = AuthenticatedArtifact {
        artifact_hash: &hash,
        producer_id: "scip-test",
        producer_version: "1.0.0",
        source_set_id: "app",
        source_revision: "r1",
        position_encoding: 1,
        protocol_version: 0,
        text_document_encoding: 1,
        project_root_uri: "file:///synthetic",
        module_roots: &super::ModuleRoots::default(),
        cargo_targets: &super::CargoTargets::default(),
        assigned_documents: 0,
    };
    let result = decode_streaming(
        bytes.as_slice(),
        bytes.len() as u64,
        &artifact,
        &mut |_: &str, _: &str| Err(invalid("unlisted document")),
        &mut BatchCounters::default(),
        &limits,
        &mut CountingSink::default(),
        &flag(),
    );
    assert_eq!(result, Err(limit("index entries")));
}

#[test]
fn documents_missing_from_the_artifact_are_rejected_at_the_end() {
    let bytes = hex::decode(FULL).unwrap();
    let mut docs = vector_documents(FULL);
    docs.push(AuthenticatedScipDocument {
        language: "javascript".into(),
        relative_path: "src/C.js".into(),
        source_bytes: SOURCE,
        content_hash: sha(SOURCE),
    });
    assert!(matches!(
        streamed(&vector_input(&bytes, docs)),
        Err(ScipCaptureError::InvalidArtifact(reason)) if reason == "missing artifact document"
    ));
}
