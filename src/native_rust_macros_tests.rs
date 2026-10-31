//! Decision 0019 (#519): allowlisted Rust expression macros are measured as expressions.
use super::*;

fn rust(path: &str, text: &str) -> SourceFile {
    SourceFile {
        path: path.into(),
        hash: hash(text.as_bytes()),
        language: "rust".into(),
        text: text.into(),
    }
}

/// One document measured by the production producer descriptor, with or without expansion.
fn measure(file: &SourceFile, expand_macros: bool) -> Artifact {
    let source_set = SourceSet {
        id: "source-set:v1:root".into(),
        root_id: "root".into(),
        languages: LANGUAGES.map(str::to_owned).into(),
        dependencies: vec![],
    };
    let revision_id = &format!("revision:v2:{}", "4".repeat(64));
    let document = Document {
        key: DocumentKey {
            source_set_id: source_set.id.clone(),
            language: file.language.clone(),
            path: file.path.clone(),
        },
        revision_id: revision_id.clone(),
        content_hash: file.hash.clone(),
        byte_length: file.text.len(),
    };
    let mut artifact = Artifact {
        producer: Producer {
            id: PRODUCER.into(),
            version: NATIVE_VERSION.into(),
            executable_hash: "e".repeat(64),
            kind: "native".into(),
            languages: LANGUAGES.map(str::to_owned).into(),
            position_encoding: "utf8".into(),
        },
        source_set: source_set.clone(),
        revision: Revision {
            id: revision_id.clone(),
            source_set_id: source_set.id,
            documents: vec![document.clone()],
            toolchain_hash: "1".repeat(64),
            config_hash: "2".repeat(64),
            dependency_hash: "3".repeat(64),
        },
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
        references: vec![],
        capability_reports: vec![],
        search_cards: Vec::new(),
    };
    extract_document(
        &mut artifact,
        file,
        &document,
        &mut IdentityRegistry::default(),
        &mut |_, _, _| {},
        expand_macros,
    )
    .unwrap();
    artifact
        .validate_structure(std::slice::from_ref(file))
        .unwrap();
    artifact
}

fn span(text: &str, needle: &str) -> Range {
    let start = text
        .find(needle)
        .unwrap_or_else(|| panic!("{needle} not in source"));
    assert_eq!(text.matches(needle).count(), 1, "{needle} is ambiguous");
    Range {
        start,
        end: start + needle.len(),
    }
}

fn gaps(artifact: &Artifact) -> &[MeasurementGap] {
    &artifact.capability_reports[0].families[0].gaps
}

fn declaration<'a>(artifact: &'a Artifact, name: &str) -> &'a Declaration {
    artifact
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some(name))
        .unwrap()
}

const FORMS: &str = r#"fn helper(x: i32) -> i32 { x }
fn run(f: &mut String) -> anyhow::Result<()> {
    assert!(helper(1) > 0);
    assert_eq!(helper(2), 2, "two {}", helper(3));
    std::assert_ne!(helper(4), 0);
    debug_assert!(helper(5) > 0);
    debug_assert_eq!(helper(6), 6);
    debug_assert_ne!(helper(7), 0);
    anyhow::ensure!(helper(8) > 0, "eight {}", helper(9));
    ensure!(helper(10) > 0);
    let _ = format!("{}", helper(11));
    let _ = format_args!("{:?}", helper(12));
    write!(f, "{}", helper(13))?;
    writeln!(f, "{}", helper(14))?;
    print!("{}", helper(15));
    println!("{}", helper(16));
    eprint!("{}", helper(17));
    eprintln!("{}", helper(18));
    let _ = vec![helper(19), helper(20)];
    let _ = vec![helper(21); 2];
    let _ = dbg!(helper(22));
    let _ = anyhow::anyhow!("{}", helper(23));
    if f.is_empty() { panic!("{}", helper(24)); }
    if f.is_empty() { unreachable!("{}", helper(25)); }
    if f.is_empty() { todo!("{}", helper(26)); }
    if f.is_empty() { bail!("{}", helper(27)); }
    let _ = vec! { helper(28) };
    assert![helper(29) > 0];
    Ok(())
}
"#;

#[test]
fn calls_inside_each_allowlisted_form_are_measured_at_exact_spans_with_their_owner() {
    let file = rust("src/forms.rs", FORMS);
    let artifact = measure(&file, true);
    let run = declaration(&artifact, "run");
    let regions: BTreeSet<_> = artifact
        .control_regions
        .iter()
        .map(|r| r.id.as_str())
        .collect();
    for n in 1..=29 {
        let call = format!("helper({n})");
        let range = span(FORMS, &call);
        let measured = artifact
            .calls
            .iter()
            .find(|c| c.range == range)
            .unwrap_or_else(|| panic!("{call} not measured"));
        assert_eq!(measured.owner_syntax_id, run.syntax_id, "{call}");
        assert_eq!(measured.spelling.as_deref(), Some("helper"), "{call}");
        assert_eq!(
            measured.callee_range,
            Some(Range {
                start: range.start,
                end: range.start + "helper".len(),
            }),
            "{call}"
        );
        // Calls in `if` bodies carry the enclosing region, like ordinary calls.
        assert_eq!(
            !measured.region_ids.is_empty(),
            (24..=27).contains(&n),
            "{call}"
        );
        assert!(
            measured
                .region_ids
                .iter()
                .all(|id| regions.contains(id.as_str()))
        );
        let reference = artifact
            .references
            .iter()
            .find(|r| r.range.start == range.start && r.spelling == "helper")
            .unwrap_or_else(|| panic!("{call} callee reference not measured"));
        assert_eq!(reference.owner_syntax_id, run.syntax_id);
    }
    // Macro paths are measured references; `ensure!` without a path is one too.
    for path in ["anyhow::ensure", "std::assert_ne"] {
        let range = span(FORMS, path);
        let (module, name) = path.split_once("::").unwrap();
        for (spelling, start) in [(module, range.start), (name, range.end - name.len())] {
            assert!(
                artifact
                    .references
                    .iter()
                    .any(|r| r.spelling == spelling && r.range.start == start),
                "{spelling}"
            );
        }
    }
    let report = &artifact.capability_reports[0];
    assert_eq!(report.macro_expansions.len(), 26);
    assert!(gaps(&artifact).is_empty(), "{:?}", gaps(&artifact));
    assert_eq!(artifact.coverage[0].state, "complete");
}

#[test]
fn format_strings_stay_opaque_where_they_name_arguments() {
    let text = r#"fn helper(x: i32) -> i32 { x }
fn run(width: usize) -> String {
    let x = helper(0);
    let a = format!("{x} {}", helper(1));
    let b = format!("{:>width$}", helper(2));
    let c = format!("{} {value}", helper(3), value = helper(4));
    let d = format!(r"{}", helper(5));
    format!("{a}{b}{c}{d}")
}
"#;
    let file = rust("src/format.rs", text);
    let artifact = measure(&file, true);
    for n in 1..=5 {
        let range = span(text, &format!("helper({n})"));
        assert!(
            artifact.calls.iter().any(|c| c.range == range),
            "helper({n})"
        );
    }
    let opaque: Vec<_> = gaps(&artifact).iter().map(|g| g.range.clone()).collect();
    assert_eq!(
        opaque,
        vec![
            span(text, r#""{x} {}""#),
            span(text, r#""{:>width$}""#),
            span(text, r#""{} {value}""#),
            span(text, "value ="),
            span(text, r#""{a}{b}{c}{d}""#),
        ]
        .into_iter()
        .map(|mut range| {
            // `value =` names the argument; only the identifier is opaque.
            if &text[range.start..range.end] == "value =" {
                range.end = range.start + "value".len();
            }
            range
        })
        .collect::<Vec<_>>()
    );
    // No candidate is measured inside an opaque format string or argument name.
    for gap in gaps(&artifact) {
        assert!(
            !artifact
                .references
                .iter()
                .any(|r| gap.range.start <= r.range.start && r.range.end <= gap.range.end)
        );
    }
    assert_eq!(artifact.coverage[0].state, "partial");
}

#[test]
fn excluded_and_unparseable_macros_stay_whole_gaps() {
    let text = r#"fn helper(x: i32) -> i32 { x }
macro_rules! local { () => { helper(1) }; }
#[cfg(any())]
fn run(v: i32) {
    let _ = json!({ "a": helper(2) });
    let _ = matches!(helper(3), 1);
    select! { x = helper(4) => {} }
    local!(helper(5));
    assert!(helper(6) => 1);
    assert!(matches!(helper(7), 1));
    assert!(vec![helper(8)].len() == 1);
    other::assert!(helper(9));
}
"#;
    let file = rust("src/excluded.rs", text);
    let artifact = measure(&file, true);
    let measured = |n: usize| {
        let range = span(text, &format!("helper({n})"));
        artifact.calls.iter().any(|c| c.range == range)
    };
    for n in [1, 2, 3, 4, 5, 6, 7, 9] {
        assert!(!measured(n), "helper({n}) must stay opaque");
    }
    assert!(measured(8), "a nested allowlisted macro is measured");
    let gap_ranges: Vec<_> = gaps(&artifact).iter().map(|g| g.range.clone()).collect();
    for construct in [
        "macro_rules! local { () => { helper(1) }; }",
        "#[cfg(any())]",
        r#"json!({ "a": helper(2) })"#,
        "matches!(helper(3), 1)",
        "select! { x = helper(4) => {} }",
        "local!(helper(5))",
        "assert!(helper(6) => 1)",
        "matches!(helper(7), 1)",
        "other::assert!(helper(9))",
    ] {
        assert!(gap_ranges.contains(&span(text, construct)), "{construct}");
    }
    let unparseable = gaps(&artifact)
        .iter()
        .find(|g| g.range == span(text, "assert!(helper(6) => 1)"))
        .unwrap();
    assert!(
        unparseable
            .diagnostic
            .contains("not measurable as Rust expressions")
    );
    assert_eq!(artifact.coverage[0].state, "partial");
}

/// Expansion adds facts only inside expanded macros. Every declaration, call, region and
/// reference outside them keeps its ID, owner and ordinal, under the same producer.
fn assert_unchanged_outside_macros(file: &SourceFile) {
    let opaque = measure(file, false);
    let expanded = measure(file, true);
    let spans = &expanded.capability_reports[0].macro_expansions;
    let outside = |r: &Range| !in_macro_expansion(spans, r);
    let json = |value: &dyn erased::Json| value.to_value();
    let keep = |rows: Vec<Value>, ranges: Vec<Range>| -> Vec<Value> {
        rows.into_iter()
            .zip(ranges)
            .filter(|(_, range)| outside(range))
            .map(|(row, _)| row)
            .collect()
    };
    macro_rules! same {
        ($family:ident) => {{
            let before: Vec<Value> = opaque.$family.iter().map(|x| json(x)).collect();
            let after = keep(
                expanded.$family.iter().map(|x| json(x)).collect(),
                expanded.$family.iter().map(|x| x.range.clone()).collect(),
            );
            let sort = |mut rows: Vec<Value>| {
                rows.sort_by_key(|row| row.to_string());
                rows
            };
            assert_eq!(
                sort(before),
                sort(after),
                "{} differ outside macros",
                stringify!($family)
            );
        }};
    }
    same!(declarations);
    same!(calls);
    same!(control_regions);
    same!(references);
    // Gaps differ only at the expanded macros themselves.
    let before: Vec<_> = gaps(&opaque)
        .iter()
        .filter(|g| !spans.contains(&g.range))
        .collect();
    let after: Vec<_> = gaps(&expanded)
        .iter()
        .filter(|g| outside(&g.range))
        .collect();
    assert_eq!(before, after);
    assert!(opaque.capability_reports[0].macro_expansions.is_empty());
}

mod erased {
    pub(super) trait Json {
        fn to_value(&self) -> serde_json::Value;
    }
    impl<T: serde::Serialize> Json for T {
        fn to_value(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap()
        }
    }
}

#[test]
fn ids_outside_macros_are_unchanged_for_a_corpus() {
    // Macros precede ordinary facts of the same owner, closures inside macros precede
    // ordinary sibling closures, and regions nest around both.
    let synthetic = r#"fn g(x: i32) -> i32 { x }
fn f(v: Vec<i32>) -> i32 {
    assert!(v.iter().all(|x| g(*x) > 0), "{}", g(1));
    let h = |y: i32| g(y);
    if v.is_empty() { assert_eq!(g(2), h(3)); g(4) } else { h(g(5)) }
}
"#;
    let corpus = [
        rust("src/synthetic.rs", synthetic),
        rust("src/native_ids.rs", include_str!("native_ids.rs")),
        rust(
            "src/native_rust_macros.rs",
            include_str!("native_rust_macros.rs"),
        ),
        rust("src/store/anchors.rs", include_str!("store/anchors.rs")),
    ];
    for file in &corpus {
        assert_unchanged_outside_macros(file);
    }
    // The synthetic document really exercises the late ordinals.
    let expanded = measure(&corpus[0], true);
    let closures: Vec<_> = expanded
        .declarations
        .iter()
        .filter(|d| d.kind == "anonymousFunction")
        .map(|d| (d.key.ordinal, &synthetic[d.range.start..d.range.end]))
        .collect();
    assert_eq!(closures, vec![(0, "|y: i32| g(y)"), (1, "|x| g(*x) > 0")]);
}

#[test]
fn hostile_macro_input_is_bounded_and_never_fails_the_document() {
    // Nesting deeper than the walk's depth bound stays one opaque gap.
    let depth = 300;
    let deep = format!(
        "fn f() {{ let _ = {}1{}; }}\n",
        "vec![".repeat(depth),
        "]".repeat(depth)
    );
    let file = rust("src/deep.rs", &deep);
    let artifact = measure(&file, true);
    assert!(artifact.capability_reports[0].macro_expansions.is_empty());
    assert_eq!(gaps(&artifact).len(), 1);
    // Deep nesting of measurable macros stops at the depth bound, leaving one inner gap.
    let nested = format!(
        "fn f() {{ {}g(){}; }}\n",
        "assert!(".repeat(100),
        ")".repeat(100)
    );
    let artifact = measure(&rust("src/nested.rs", &nested), true);
    assert_eq!(artifact.capability_reports[0].macro_expansions.len(), 1);
    assert_eq!(gaps(&artifact).len(), 1);
    // Re-parsing nested macros is bounded by the per-document byte budget: each level
    // re-reads the large payload, so the innermost levels stay opaque.
    let levels = 14;
    let payload = (0..8000)
        .map(|i| format!("g({i})"))
        .collect::<Vec<_>>()
        .join(" + ");
    let nested = format!(
        "fn g(x: i32) -> i32 {{ x }}\nfn f() {{ {}{payload} > 0{}; }}\n",
        "assert!(".repeat(levels),
        ")".repeat(levels)
    );
    let artifact = measure(&rust("src/budget.rs", &nested), true);
    assert_eq!(artifact.capability_reports[0].macro_expansions.len(), 1);
    let opaque = gaps(&artifact);
    assert_eq!(opaque.len(), 1, "{opaque:?}");
    assert!(opaque[0].diagnostic.contains("within native bounds"));
    // The budget (8 x document + 64 KiB) admits about nine nested re-reads of the payload.
    let depth_of_gap = nested[..opaque[0].range.start].matches("assert!(").count();
    assert!((2..levels).contains(&depth_of_gap), "{depth_of_gap}");
    assert!(
        artifact.calls.is_empty(),
        "payload calls lie inside the opaque level"
    );
    // Arbitrary token trees never panic and stay opaque when they are not expressions.
    for body in [
        "assert!(@ # $ ~);",
        "format!(=> => =>);",
        "vec![; ;];",
        "dbg!(fn x() {});",
        "assert!(\"\\u{1F600}\", é);",
        "ensure!(a b c);",
        "println!(r#\"{ \"#);",
    ] {
        let text = format!("fn f() {{ {body} }}\n");
        let artifact = measure(&rust("src/garbage.rs", &text), true);
        assert!(artifact.calls.is_empty(), "{body}");
    }
}

#[test]
fn qualified_paths_with_whitespace_and_comments_are_allowlisted() {
    let text = "fn g() -> i32 { 1 }\nfn f(w: &mut String) -> anyhow::Result<()> {\n    std :: assert!(g() > 0);\n    core::/* c */assert_eq!(g(), 1);\n    ::alloc\n        ::vec![g()];\n    anyhow:: ensure!(g() > 0);\n    std::write!(w, \"{}\", g())?;\n    std::other!(g());\n    Ok(())\n}\n";
    let artifact = measure(&rust("src/paths.rs", text), true);
    let calls = artifact
        .calls
        .iter()
        .filter(|c| c.spelling.as_deref() == Some("g"))
        .count();
    assert_eq!(calls, 5);
    assert_eq!(artifact.capability_reports[0].macro_expansions.len(), 5);
    assert_eq!(gaps(&artifact).len(), 1, "only std::other! stays opaque");
}

#[test]
fn escaped_format_braces_keep_the_capture_gap() {
    let text = "fn run() -> String { let value = 1; format!(\"\\u{07b}value}\") }\n";
    let artifact = measure(&rust("src/escaped.rs", text), true);
    let literal = span(text, "\"\\u{07b}value}\"");
    assert_eq!(
        gaps(&artifact)
            .iter()
            .map(|g| g.range.clone())
            .collect::<Vec<_>>(),
        vec![literal]
    );
    assert_eq!(artifact.coverage[0].state, "partial");
}
