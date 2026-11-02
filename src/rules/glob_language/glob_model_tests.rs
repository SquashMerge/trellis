//! Independent executable model of the admitted rules path language.
//!
//! Run with `TRELLIS_GLOB_CASES=10000 cargo test --locked --lib glob_model_tests`.
//! The reference operates on path components and byte slices; it does not parse
//! production patterns or use globset/NFA helpers.
use super::*;
use crate::rules::{self, LayerMatcher};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, RngSeed, TestRunner};

#[derive(Clone, Debug)]
enum Atom {
    Lit(&'static str),
    Star,
    One,
}

#[derive(Clone, Debug)]
enum Segment {
    Recursive,
    Parts(Vec<Atom>),
}

type Pattern = Vec<Segment>;

fn render(pattern: &[Segment]) -> String {
    pattern
        .iter()
        .map(|segment| match segment {
            Segment::Recursive => "**".to_owned(),
            Segment::Parts(parts) => parts
                .iter()
                .map(|part| match part {
                    Atom::Lit(s) => *s,
                    Atom::Star => "*",
                    Atom::One => "?",
                })
                .collect(),
        })
        .collect::<Vec<String>>()
        .join("/")
}

fn component(parts: &[Atom], bytes: &[u8]) -> bool {
    match parts.split_first() {
        None => bytes.is_empty(),
        Some((Atom::Lit(s), rest)) => bytes
            .strip_prefix(s.as_bytes())
            .is_some_and(|tail| component(rest, tail)),
        Some((Atom::One, rest)) => !bytes.is_empty() && component(rest, &bytes[1..]),
        Some((Atom::Star, rest)) => (0..=bytes.len()).any(|n| component(rest, &bytes[n..])),
    }
}

fn reference(pattern: &[Segment], path: &str) -> bool {
    // A terminal ** means descendants, except that a pattern consisting solely
    // of recursive segments covers every valid path.
    fn walk(pattern: &[Segment], path: &[&str], at_start: bool) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((Segment::Recursive, rest)) => {
                let rest = rest
                    .iter()
                    .skip_while(|s| matches!(s, Segment::Recursive))
                    .collect::<Vec<_>>();
                if rest.is_empty() {
                    return at_start || !path.is_empty();
                }
                (0..=path.len()).any(|n| walk_refs(&rest, &path[n..]))
            }
            Some((Segment::Parts(parts), rest)) => {
                !path.is_empty()
                    && component(parts, path[0].as_bytes())
                    && walk(rest, &path[1..], false)
            }
        }
    }
    fn walk_refs(pattern: &[&Segment], path: &[&str]) -> bool {
        let owned: Vec<Segment> = pattern.iter().map(|p| (*p).clone()).collect();
        walk(&owned, path, false)
    }
    walk(pattern, &path.split('/').collect::<Vec<_>>(), true)
}

fn config(pattern: &str) -> anyhow::Result<rules::Config> {
    // JSON string escaping is a valid TOML basic-string spelling for our inputs.
    let literal = serde_json::to_string(pattern).unwrap();
    rules::parse(&format!(
        "version = 1\n[layers.test]\norder = 1\npaths = [{literal}]\n"
    ))
}

fn nfa_accepts(nfa: &Nfa, path: &str) -> bool {
    let mut budget = Budget {
        work: &mut Work::new(work::rules_limit(0, 0)),
        check: &mut || Ok(()),
    };
    let mut state = nfa.start(&mut budget).unwrap();
    for b in path.bytes() {
        state = nfa.advance(&state, b, &mut budget).unwrap();
    }
    state.contains(&nfa.accept)
}

fn compile(pattern: &str) -> Nfa {
    Nfa::compile(
        pattern,
        &mut Budget {
            work: &mut Work::new(work::rules_limit(0, 0)),
            check: &mut || Ok(()),
        },
    )
    .unwrap()
}

fn atom() -> impl Strategy<Value = Atom> {
    prop_oneof![5 => prop::sample::select(vec!["a", "b", "A", ".", "é", "中", "😀", "\n"]).prop_map(Atom::Lit), 2 => Just(Atom::Star), 2 => Just(Atom::One)]
}

fn pattern() -> impl Strategy<Value = Pattern> {
    // Interleave literal barriers so rendered adjacent stars never become **.
    let parts = prop::collection::vec(atom(), 1..4).prop_map(|atoms| {
        let mut out = Vec::new();
        for atom in atoms {
            if matches!(out.last(), Some(Atom::Star)) && matches!(atom, Atom::Star) {
                out.push(Atom::Lit("a"));
            }
            out.push(atom);
        }
        // Literal dot and dot-dot are invalid pattern components.
        if matches!(
            out.as_slice(),
            [Atom::Lit(".")] | [Atom::Lit("."), Atom::Lit(".")]
        ) {
            out.push(Atom::Lit("a"));
        }
        Segment::Parts(out)
    });
    prop::collection::vec(prop_oneof![2 => Just(Segment::Recursive), 5 => parts], 1..5)
}

fn witness(pattern: &[Segment], deep: bool) -> String {
    let mut segments = Vec::new();
    for (i, s) in pattern.iter().enumerate() {
        match s {
            Segment::Recursive if deep || i + 1 == pattern.len() => segments.push("x".to_string()),
            Segment::Recursive => {}
            Segment::Parts(parts) => {
                let mut s: String = parts
                    .iter()
                    .map(|p| match p {
                        Atom::Lit(s) => *s,
                        Atom::Star | Atom::One => "a",
                    })
                    .collect();
                if s == "." || s == ".." {
                    s.push('a');
                }
                segments.push(s);
            }
        }
    }
    if segments.is_empty() {
        segments.push("x".into());
    }
    segments.join("/")
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 8192
        && !path.contains(['\0', '\\', ':'])
        && path.split('/').all(|p| !matches!(p, "" | "." | ".."))
}

fn check_pair(pattern: &[Segment], paths: &[String]) -> (usize, usize) {
    let text = render(pattern);
    let cfg = config(&text).unwrap_or_else(|e| panic!("valid generated pattern {text:?}: {e}"));
    let matcher = &cfg.layers["test"].patterns[0].matcher;
    let nfa = compile(&text);
    let mut counts = (0, 0);
    for path in paths.iter().filter(|p| valid_path(p)) {
        let expected = reference(pattern, path);
        if expected {
            counts.0 += 1;
        } else {
            counts.1 += 1;
        }
        assert_eq!(
            matcher.is_match(path),
            expected,
            "rules matcher: {text:?} / {path:?}"
        );
        assert_eq!(
            nfa_accepts(&nfa, path),
            expected,
            "language NFA: {text:?} / {path:?}"
        );
    }
    counts
}

fn arbitrary_path() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!["a", "b", "A", ".a", "...", "é", "中", "😀", "\n"]),
        1..5,
    )
    .prop_map(|p| p.join("/"))
}

fn cases() -> u32 {
    std::env::var("TRELLIS_GLOB_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/proptest-regressions/glob_model.txt"))), ..ProptestConfig::default() })]

    #[test]
    fn generated_matcher_and_nfa(pattern in pattern(), other in arbitrary_path()) {
        let zero = witness(&pattern, false);
        let deep = witness(&pattern, true);
        // Include altered slash boundaries, case, and hidden names without normalization.
        let paths = vec![zero.clone(), deep.clone(), other, format!("{zero}/a"), format!("a/{zero}"), zero.to_uppercase(), format!(".{deep}")];
        prop_assert!(reference(&pattern, &deep), "constructive witness failed: {:?} / {:?}", pattern, deep);
        let (yes, _) = check_pair(&pattern, &paths);
        prop_assert!(yes > 0);
    }

    #[test]
    fn malformed_patterns_are_located_glob_errors(
        left in "[a-zA-Z0-9]{0,12}", right in "[a-zA-Z0-9]{0,12}",
        defect in prop::sample::select(vec!["[a]", "[", "]", "{a,b}", "!", "\\", ":", "\0", "//", "/./", "/../", "a**", "**b", "***"])
    ) {
        let raw = format!("x{left}{defect}{right}z");
        let err = match config(&raw) { Ok(_) => return Err(TestCaseError::fail(format!("accepted {raw:?}"))), Err(e) => e };
        let diagnostic = err.downcast_ref::<rules::RuleError>().expect("typed rule error");
        prop_assert_eq!((&*diagnostic.code, &*diagnostic.kind, &*diagnostic.attribution), ("invalid_rules", "glob", "token"));
        prop_assert!(!crate::references::valid_path_glob(&raw));
    }

    #[test]
    fn untrusted_pattern_text_never_panics(raw in prop::collection::vec(any::<char>(), 0..100).prop_map(|v| v.into_iter().collect::<String>())) {
        match config(&raw) {
            Ok(_) => prop_assert!(crate::references::valid_path_glob(&raw), "rules accepted {raw:?}"),
            Err(e) => prop_assert!(e.downcast_ref::<rules::RuleError>().is_some(), "untyped: {e:#}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_GLOB_RELATION_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(64), ..ProptestConfig::default() })]

    #[test]
    fn relation_to_literal_decides_model_membership(pattern in pattern(), path in arbitrary_path()) {
        let text = render(&pattern);
        let result = relation(&text, &path, &mut Work::new(work::rules_limit(0, 0)), &mut || Ok(())).unwrap();
        let contains = matches!(result, Relation::Equivalent | Relation::StrictSuperset);
        prop_assert_eq!(contains, reference(&pattern, &path), "pattern {:?}, path {:?}, relation {:?}", text, path, result);
    }

    #[test]
    fn relation_never_contradicts_generated_witnesses(a in pattern(), b in pattern(), other in arbitrary_path()) {
        let at = render(&a);
        let bt = render(&b);
        let r = relation(&at, &bt, &mut Work::new(work::rules_limit(0, 0)), &mut || Ok(())).unwrap();
        for path in [witness(&a, false), witness(&a, true), witness(&b, false), witness(&b, true), other] {
            let (in_a, in_b) = (reference(&a, &path), reference(&b, &path));
            match r {
                Relation::Equivalent => prop_assert_eq!(in_a, in_b, "{:?} vs {:?} at {:?}", at, bt, path),
                Relation::StrictSubset => prop_assert!(!in_a || in_b, "{:?} subset {:?} at {:?}", at, bt, path),
                Relation::StrictSuperset => prop_assert!(!in_b || in_a, "{:?} superset {:?} at {:?}", at, bt, path),
                Relation::Incomparable => {},
            }
        }
    }
}

#[test]
fn invalid_path_pattern_boundaries_are_typed() {
    for raw in [
        "", "/a", "a/", "a//b", ".", "..", "a/./b", "a/../b", "[a]", "a\\*", "!a", "a:b", "a\0b",
        "a**b",
    ] {
        let e = match config(raw) {
            Ok(_) => panic!("accepted {raw:?}"),
            Err(e) => e,
        };
        let diagnostic = e.downcast_ref::<rules::RuleError>().unwrap();
        assert_eq!(
            (
                &*diagnostic.code,
                &*diagnostic.kind,
                &*diagnostic.attribution
            ),
            ("invalid_rules", "glob", "token"),
            "{raw:?}"
        );
    }
    let too_large = format!("version = 1\n#{}", "a".repeat(2 * 1024 * 1024));
    let e = match rules::parse(&too_large) {
        Ok(_) => panic!("accepted oversized config"),
        Err(e) => e,
    };
    assert_eq!(
        e.downcast_ref::<rules::RuleError>().unwrap().kind,
        "too_large"
    );
}

#[test]
fn sampled_generator_coverage_is_non_vacuous() {
    let mut runner = TestRunner::new(Config {
        rng_seed: RngSeed::Fixed(0x617_610b),
        ..Config::default()
    });
    let strategy = (pattern(), arbitrary_path());
    let (mut recursive, mut star, mut one, mut unicode, mut yes, mut no) = (0, 0, 0, 0, 0, 0);
    for _ in 0..512 {
        let (pattern, path) = strategy.new_tree(&mut runner).unwrap().current();
        let text = render(&pattern);
        recursive += usize::from(pattern.iter().any(|p| matches!(p, Segment::Recursive)));
        star +=
            usize::from(pattern.iter().any(
                |p| matches!(p, Segment::Parts(a) if a.iter().any(|a| matches!(a, Atom::Star))),
            ));
        one += usize::from(text.contains('?'));
        unicode += usize::from(!text.is_ascii());
        for p in [witness(&pattern, false), witness(&pattern, true), path] {
            if reference(&pattern, &p) {
                yes += 1;
            } else {
                no += 1;
            }
        }
    }
    eprintln!(
        "sampled 512: recursive={recursive}, star={star}, question={one}, unicode={unicode}, matches={yes}, misses={no}"
    );
    assert!(
        [recursive, star, one, unicode, yes, no]
            .into_iter()
            .all(|n| n >= 30)
    );
}

#[test]
fn deterministic_exhaustive_small_domain_and_witnesses() {
    let atoms = [
        Atom::Lit("a"),
        Atom::Lit("."),
        Atom::Lit("é"),
        Atom::Star,
        Atom::One,
    ];
    let mut segments = vec![Segment::Recursive];
    for a in &atoms {
        if !matches!(a, Atom::Lit(".")) {
            segments.push(Segment::Parts(vec![a.clone()]));
        }
        for b in &atoms {
            if matches!(
                (a, b),
                (Atom::Star, Atom::Star) | (Atom::Lit("."), Atom::Lit("."))
            ) {
                continue;
            }
            segments.push(Segment::Parts(vec![a.clone(), b.clone()]));
        }
    }
    let mut paths = Vec::new();
    let mut frontier = vec![String::new()];
    for _ in 0..4 {
        let mut next = Vec::new();
        for prefix in frontier {
            for suffix in ["a", ".", "/", "é"] {
                let p = format!("{prefix}{suffix}");
                if valid_path(&p) {
                    paths.push(p.clone());
                }
                next.push(p);
            }
        }
        frontier = next;
    }
    let mut totals = (0, 0);
    for a in &segments {
        for pattern in [
            vec![a.clone()],
            vec![a.clone(), Segment::Recursive],
            vec![Segment::Recursive, a.clone()],
            vec![
                Segment::Parts(vec![Atom::Lit("a")]),
                Segment::Recursive,
                a.clone(),
            ],
        ] {
            let (yes, no) = check_pair(&pattern, &paths);
            totals.0 += yes;
            totals.1 += no;
        }
    }
    eprintln!(
        "exhaustive: {} paths, {} matches, {} nonmatches",
        paths.len(),
        totals.0,
        totals.1
    );
    assert!(totals.0 > 100 && totals.1 > 100);
    for (pat, path, expected) in [
        ("*", "a/b", false),
        ("a/**/b", "a/b", true),
        ("a/**", "a", false),
        ("?", "é", false),
        ("??", "é", true),
        ("*", ".a", true),
    ] {
        let cfg = config(pat).unwrap();
        assert_eq!(
            cfg.layers["test"].patterns[0].matcher.is_match(path),
            expected,
            "{pat:?} / {path:?}"
        );
        assert_eq!(
            nfa_accepts(&compile(pat), path),
            expected,
            "{pat:?} / {path:?}"
        );
    }
}

#[test]
fn layer_selection_witnesses() {
    for (a, b, path, expected) in [
        ("src/?", "src/*", "src/a", Some("a")),
        ("src/*", "src/?*", "src/a", None),
        ("src/a*", "src/*a", "src/aa", None),
        ("src/a", "src/*", "src/a", Some("a")),
        ("src/**/b", "src/*/b", "src/a/b", Some("b")),
    ] {
        let text = format!(
            "version = 1\n[layers.a]\norder = 1\npaths = [{a:?}]\n[layers.b]\norder = 2\npaths = [{b:?}]\n"
        );
        let cfg = rules::parse(&text).unwrap();
        let result = LayerMatcher::new(&cfg).select(
            path,
            &mut Work::new(work::rules_limit(0, 0)),
            &mut || Ok(()),
        );
        match expected {
            Some(layer) => assert_eq!(result.unwrap(), Some(layer)),
            None => assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<rules::RuleError>()
                    .unwrap()
                    .kind,
                "ambiguity"
            ),
        }
    }
}
