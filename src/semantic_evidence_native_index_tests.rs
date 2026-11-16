use super::*;

const PREFIX: &str = "fn main() {\n";

fn hex(n: usize) -> String {
    format!("{n:032x}")
}

fn document() -> native::DocumentKey {
    native::DocumentKey {
        source_set_id: "set".into(),
        language: "rust".into(),
        path: "src/lib.rs".into(),
    }
}

fn line(i: usize) -> usize {
    PREFIX.len() + 5 * i
}

fn range(start: usize, end: usize) -> native::Range {
    native::Range { start, end }
}

/// A Rust document of `n` lines `f();` inside `main`, each line a measured call with callee
/// `f` and a reference `f`; every fourth line's `;\n` is a callee capability gap.
fn artifact(n: usize) -> (native::Artifact, Vec<u8>) {
    let mut text = String::from(PREFIX);
    for _ in 0..n {
        text.push_str("f();\n");
    }
    text.push_str("}\n");
    let key = document();
    let revision = "revision:test".to_string();
    let owner = format!("sid:v1:{}", hex(0));
    let declaration = native::Declaration {
        role: native::DeclarationRole::Production,
        syntax_id: owner.clone(),
        document: key.clone(),
        revision_id: revision.clone(),
        kind: "function".into(),
        name: Some("main".into()),
        lookup_key: Some("main".into()),
        ancestors: vec![],
        key: native::Key {
            kind: "function".into(),
            name: Some("main".into()),
            signature: None,
            ordinal: 0,
        },
        range: range(0, text.len()),
        name_range: Some(range(3, 7)),
        header: native::Header {
            kind: "function".into(),
            name: Some("main".into()),
            modifiers: vec![],
            type_parameters: vec![],
            parameters: vec![],
            result_type: None,
            bases: vec![],
        },
        provenance_id: "native".into(),
    };
    let calls = (0..n)
        .map(|i| native::Call {
            id: format!("occ:v2:{}", hex(i)),
            owner_syntax_id: owner.clone(),
            ordinal: i,
            document: key.clone(),
            revision_id: revision.clone(),
            range: range(line(i), line(i) + 3),
            callee_range: Some(range(line(i), line(i) + 1)),
            spelling: Some("f".into()),
            region_ids: vec![],
            provenance_id: "native".into(),
        })
        .collect();
    let references = (0..n)
        .map(|i| native::Reference {
            id: format!("occ:v2:{}", hex(n + i)),
            owner_syntax_id: owner.clone(),
            ordinal: i,
            document: key.clone(),
            revision_id: revision.clone(),
            range: range(line(i), line(i) + 1),
            spelling: "f".into(),
            lookup_key: "f".into(),
            provenance_id: "native".into(),
        })
        .collect();
    let family = |kind, gaps| native::MeasurementSupport {
        kind,
        available: true,
        diagnostic: None,
        gaps,
    };
    let gaps = (0..n)
        .step_by(4)
        .map(|i| native::MeasurementGap {
            range: range(line(i) + 3, line(i) + 5),
            diagnostic: format!("gap {i}"),
        })
        .collect();
    let native = native::Artifact {
        producer: native::Producer {
            id: "native".into(),
            version: "1".into(),
            executable_hash: "e".repeat(64),
            kind: "native".into(),
            languages: vec!["rust".into()],
            position_encoding: "utf8".into(),
        },
        source_set: native::SourceSet {
            id: "set".into(),
            root_id: "root".into(),
            languages: vec!["rust".into()],
            dependencies: vec![],
        },
        revision: native::Revision {
            id: revision.clone(),
            source_set_id: "set".into(),
            documents: vec![native::Document {
                key: key.clone(),
                revision_id: revision.clone(),
                content_hash: "c".repeat(64),
                byte_length: text.len(),
            }],
            toolchain_hash: "t".repeat(64),
            config_hash: "c".repeat(64),
            dependency_hash: "d".repeat(64),
        },
        coverage: vec![],
        provenance: vec![],
        declarations: vec![declaration],
        calls,
        control_regions: vec![],
        references,
        capability_reports: vec![native::CapabilityReport {
            document: key,
            revision_id: revision,
            families: vec![
                family(native::MeasurementKind::DeclarationName, vec![]),
                family(native::MeasurementKind::Callee, gaps),
                family(native::MeasurementKind::Invocation, vec![]),
                family(native::MeasurementKind::Reference, vec![]),
            ],
            macro_expansions: vec![],
        }],
        search_cards: vec![],
    };
    (native, text.into_bytes())
}

fn join(
    native: &native::Artifact,
    kind: JoinKind,
    range: native::Range,
    candidates: Vec<String>,
    diagnostic: Option<String>,
) -> Join {
    let status = match (candidates.len(), &diagnostic) {
        (1, _) => JoinStatus::Exact,
        (0, Some(_)) => JoinStatus::Unsupported,
        (0, None) => JoinStatus::Unmatched,
        _ => JoinStatus::Ambiguous,
    };
    Join {
        anchor: MeasuredAnchor {
            document: document(),
            revision_id: native.revision.id.clone(),
            content_hash: native.revision.documents[0].content_hash.clone(),
            range,
            kind,
        },
        status,
        candidate_ids: candidates,
        diagnostic,
    }
}

/// Validates every join and reference row of `artifact(n)` against one index and returns
/// the validation work, the index build included.
fn validate_document(n: usize) -> u64 {
    let (native, source) = artifact(n);
    VALIDATION_WORK.with(|work| work.set(0));
    let index = NativeIndex::new(&native);
    let sources = |key: &native::DocumentKey| (*key == document()).then_some(&source[..]);
    for (i, call) in native.calls.iter().enumerate() {
        let callee = call.callee_range.clone().unwrap();
        let exact = join(
            &native,
            JoinKind::Callee,
            callee,
            vec![call.id.clone()],
            None,
        );
        validate_join(&exact, JoinKind::Callee, &native, &index, &sources).unwrap();
        let invocation = join(
            &native,
            JoinKind::Invocation,
            call.range.clone(),
            vec![call.id.clone()],
            None,
        );
        validate_join(&invocation, JoinKind::Callee, &native, &index, &sources).unwrap();
        if i % 4 == 0 {
            // The line's `;` lies in its gap and measures no callee.
            let semicolon = range(call.range.end, call.range.end + 1);
            let unsupported = join(
                &native,
                JoinKind::Callee,
                semicolon,
                vec![],
                Some(format!("gap {i}")),
            );
            validate_join(&unsupported, JoinKind::Callee, &native, &index, &sources).unwrap();
        }
    }
    let rows: Vec<_> = native
        .references
        .iter()
        .map(|r| row(r, ReferenceSite::Use, Role::Call))
        .collect();
    for row in &rows {
        let exact = join(
            &native,
            JoinKind::Reference,
            row.range.clone(),
            vec![row.id.clone()],
            None,
        );
        validate_join(&exact, JoinKind::Reference, &native, &index, &sources).unwrap();
    }
    check_references(&native, &index, &source, &rows).unwrap();
    VALIDATION_WORK.with(|work| work.get())
}

/// A reference row of the measured reference `r`.
fn row(r: &native::Reference, site: ReferenceSite, role: Role) -> Reference {
    Reference {
        id: r.id.clone(),
        owner_syntax_id: r.owner_syntax_id.clone(),
        ordinal: r.ordinal,
        document: r.document.clone(),
        revision_id: r.revision_id.clone(),
        range: r.range.clone(),
        spelling: r.spelling.clone(),
        lookup_key: r.lookup_key.clone(),
        site,
        roles: vec![role],
        resolution: Resolution::Unresolved,
        declared_target: None,
        candidates: vec![],
        provenance_id: "proof".into(),
    }
}

/// Validates reference rows of `native`'s one document with the row-local validator.
fn check_references(
    native: &native::Artifact,
    index: &NativeIndex,
    source: &[u8],
    rows: &[Reference],
) -> std::result::Result<(), String> {
    let sources = |key: &native::DocumentKey| (*key == document()).then_some(source);
    let declaration = |id: &str| native.declarations.iter().find(|d| d.syntax_id == id);
    let ids: BTreeMap<_, _> = native
        .references
        .iter()
        .map(|r| (r.id.as_str(), r))
        .collect();
    let reference = |id: &str| ids.get(id).copied();
    let known = |_: &str, _: &native::DocumentKey| Ok(false);
    let env = RowEnv {
        native,
        index,
        revision: &native.revision,
        sources: &sources,
        declaration: &declaration,
        reference: &reference,
        known: &known,
    };
    let proof = Provenance {
        id: "proof".into(),
        producer_id: "producer".into(),
        document: document(),
        revision_id: native.revision.id.clone(),
        content_hash: native.revision.documents[0].content_hash.clone(),
        evidence_kind: EvidenceKind::SemanticReference,
        basis: None,
        freshness: Freshness::Fresh,
        derived_from: None,
    };
    let mut seen = BTreeSet::new();
    for row in rows {
        validate_reference_row(row, &proof, &env, &mut seen).map_err(|e| format!("{e:#}"))?;
    }
    Ok(())
}

#[test]
fn call_roles_find_calls_with_and_without_a_callee_range() {
    let (mut native, source) = artifact(4);
    let check = |native: &native::Artifact| {
        let rows = [row(&native.references[2], ReferenceSite::Use, Role::Call)];
        check_references(native, &NativeIndex::new(native), &source, &rows)
    };
    check(&native).unwrap();
    // A call without a callee range is found by its spelling.
    native.calls[2].callee_range = None;
    check(&native).unwrap();
    for (spelling, owner) in [("g", 0), ("f", 1)] {
        let mut other = native.clone();
        other.calls[2].spelling = Some(spelling.into());
        other.calls[2].owner_syntax_id = format!("sid:v1:{}", hex(owner));
        assert_eq!(
            check(&other).unwrap_err(),
            "call role without measured invocation"
        );
    }
    // A callee range elsewhere measures no call of this reference.
    native.calls[2].callee_range = Some(range(line(2) + 1, line(2) + 2));
    assert_eq!(
        check(&native).unwrap_err(),
        "call role without measured invocation"
    );
}

#[test]
fn declaration_sites_find_the_declaration_named_at_their_range() {
    let (mut native, source) = artifact(2);
    let name = native::Reference {
        id: format!("occ:v2:{}", hex(99)),
        range: range(3, 7),
        spelling: "main".into(),
        lookup_key: "main".into(),
        ..native.references[0].clone()
    };
    native.references.push(name.clone());
    let rows = [row(&name, ReferenceSite::Declaration, Role::Definition)];
    check_references(&native, &NativeIndex::new(&native), &source, &rows).unwrap();
    native.declarations[0].name_range = Some(range(3, 6));
    native.declarations[0].name = Some("mai".into());
    assert_eq!(
        check_references(&native, &NativeIndex::new(&native), &source, &rows).unwrap_err(),
        "declaration reference site not measured"
    );
}

/// `bytes` bounds the build's peak plus every family's gap ends.
fn assert_bounded(native: &native::Artifact) {
    let (index, _, peak) = crate::test_alloc::measure_peak(|| NativeIndex::new(native));
    let ends: u64 = native
        .capability_reports
        .iter()
        .flat_map(|report| &report.families)
        .map(|family| crate::test_alloc::measure(|| SupportIndex::gap_ends(&family.gaps)).1)
        .sum();
    drop(index);
    // The estimate runs before admission, so it allocates nothing itself.
    let (bound, _, scratch) = crate::test_alloc::measure_peak(|| NativeIndex::bytes(native));
    assert_eq!(scratch, 0, "the estimate allocated");
    assert!(
        peak + ends <= bound,
        "{} documents: built {peak} + gap ends {ends} > bound {bound}",
        native.revision.documents.len()
    );
}

#[test]
fn admitted_bytes_bound_what_the_index_allocates() {
    assert_bounded(&artifact(0).0);
    assert_bounded(&artifact(1).0);
    assert_bounded(&artifact(500).0);
    // More documents than one B-tree leaf holds.
    let (one, _) = artifact(3);
    let mut many = one.clone();
    for n in 1..40 {
        let key = native::DocumentKey {
            path: format!("src/m{n}.rs"),
            ..document()
        };
        let rekey = |d: &mut native::DocumentKey| *d = key.clone();
        let mut document = one.revision.documents[0].clone();
        rekey(&mut document.key);
        many.revision.documents.push(document);
        for mut d in one.declarations.clone() {
            rekey(&mut d.document);
            many.declarations.push(d);
        }
        for mut c in one.calls.clone() {
            rekey(&mut c.document);
            many.calls.push(c);
        }
        for mut r in one.references.clone() {
            rekey(&mut r.document);
            many.references.push(r);
        }
        let mut report = one.capability_reports[0].clone();
        rekey(&mut report.document);
        many.capability_reports.push(report);
    }
    assert_bounded(&many);
}

#[test]
fn validation_work_grows_linearly_with_the_document() {
    let small = validate_document(500);
    let large = validate_document(2000);
    // Each row once rescanned its whole document (source bytes, calls, references and
    // gaps), so four times the lines cost sixteen times the work; now it is about four.
    assert!(
        large <= 9 * small / 2,
        "validation work {small} at 500 lines but {large} at 2000"
    );
}

#[test]
fn indexed_spans_match_whole_source_spans() {
    let sources: [&[u8]; 4] = [b"ab\xc3\xa9\xe6\x97\xa5x", b"", b"a\xffb", b"\xe6\x97"];
    let bounds = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, usize::MAX];
    for source in sources {
        let warm = DocumentIndex::default();
        for start in bounds {
            for end in bounds {
                let range = range(start, end);
                let expected = span(source, &range).map_err(|e| format!("{e:#}"));
                for index in [&DocumentIndex::default(), &warm] {
                    let actual = index.span(source, &range).map_err(|e| format!("{e:#}"));
                    assert_eq!(actual, expected, "{source:?} {start}..{end}");
                }
            }
        }
        if std::str::from_utf8(source).is_err() {
            let error = DocumentIndex::default()
                .span(source, &range(0, 1))
                .unwrap_err();
            assert!(error.downcast_ref::<std::str::Utf8Error>().is_some());
        }
    }
}

#[test]
fn gap_lookup_finds_the_first_containing_gap_in_order() {
    let gap = |start, end| native::MeasurementGap {
        range: range(start, end),
        diagnostic: format!("{start}..{end}"),
    };
    let layouts = [
        vec![gap(0, 3), gap(2, 9), gap(3, 5), gap(4, 6), gap(8, 9)],
        vec![gap(1, 2), gap(1, 7), gap(5, 6), gap(6, 7)],
        // Out of start order: rows scan the gaps in order.
        vec![gap(5, 6), gap(0, 9), gap(2, 4)],
        vec![],
    ];
    for gaps in layouts {
        let support = native::MeasurementSupport {
            kind: native::MeasurementKind::Callee,
            available: true,
            diagnostic: None,
            gaps,
        };
        let index = SupportIndex {
            report: 0,
            family: 0,
            gap_ends: SupportIndex::gap_ends(&support.gaps),
        };
        for start in 0..10 {
            for end in start + 1..11 {
                let linear = support
                    .gaps
                    .iter()
                    .find(|g| g.range.start <= start && end <= g.range.end);
                assert_eq!(
                    index
                        .containing(&support, &range(start, end))
                        .map(|g| &g.diagnostic),
                    linear.map(|g| &g.diagnostic),
                    "{start}..{end} in {:?}",
                    support.gaps
                );
            }
        }
    }
}

#[test]
fn indexed_joins_still_reject_invalid_sources_and_anchors_typed() {
    let (native, source) = artifact(8);
    let index = NativeIndex::new(&native);
    let call = &native.calls[1];
    let callee = call.callee_range.clone().unwrap();
    let exact = join(
        &native,
        JoinKind::Callee,
        callee,
        vec![call.id.clone()],
        None,
    );
    let check = |join: &Join, source: &[u8]| {
        let sources = |key: &native::DocumentKey| (*key == document()).then_some(source);
        validate_join(join, JoinKind::Callee, &native, &index, &sources)
            .map_err(|e| format!("{e:#}"))
    };
    // Invalid UTF-8 rejects typed before the index has decoded the source.
    let mut invalid = source.clone();
    invalid[line(1) + 2] = 0xff;
    let sources = |key: &native::DocumentKey| (*key == document()).then_some(&invalid[..]);
    let error = validate_join(&exact, JoinKind::Callee, &native, &index, &sources).unwrap_err();
    assert!(error.downcast_ref::<std::str::Utf8Error>().is_some());
    // A valid join decodes the source once; bad anchors still reject, every time.
    check(&exact, &source).unwrap();
    let with_range = |start, end| {
        let mut join = exact.clone();
        join.anchor.range = range(start, end);
        join
    };
    for _ in 0..2 {
        for (join, message) in [
            (
                with_range(source.len(), source.len() + 1),
                "invalid converted semantic span",
            ),
            (with_range(3, 3), "empty semantic span"),
            (
                with_range(usize::MAX - 1, usize::MAX),
                "semantic UInt overflow",
            ),
        ] {
            assert_eq!(check(&join, &source).unwrap_err(), message);
        }
    }
    // `é` is bytes 4..6 of `fn mén() {`: a span ending inside it is not a scalar span.
    let text = String::from_utf8(source)
        .unwrap()
        .replacen("main", "mén", 1);
    let multibyte = text.into_bytes();
    let sources = |key: &native::DocumentKey| (*key == document()).then_some(&multibyte[..]);
    let index = NativeIndex::new(&native);
    for (end, message) in [
        (5, "invalid converted semantic span"),
        (6, "join differs from measured candidates"),
    ] {
        let error = validate_join(
            &with_range(3, end),
            JoinKind::Callee,
            &native,
            &index,
            &sources,
        )
        .unwrap_err();
        assert_eq!(format!("{error:#}"), message);
    }
}

#[test]
fn indexed_joins_reject_a_contradicting_gap_on_every_join() {
    let (mut native, source) = artifact(8);
    // A gap over line 1's `f()` contains its measured callee.
    native.capability_reports[0].families[1].gaps.insert(
        1,
        native::MeasurementGap {
            range: range(line(1), line(1) + 3),
            diagnostic: "macro".into(),
        },
    );
    let index = NativeIndex::new(&native);
    let sources = |key: &native::DocumentKey| (*key == document()).then_some(&source[..]);
    for call in &native.calls {
        let callee = call.callee_range.clone().unwrap();
        let exact = join(
            &native,
            JoinKind::Callee,
            callee,
            vec![call.id.clone()],
            None,
        );
        let error = validate_join(&exact, JoinKind::Callee, &native, &index, &sources).unwrap_err();
        assert_eq!(
            error.to_string(),
            "measured candidate contradicts native capability gap"
        );
    }
}
