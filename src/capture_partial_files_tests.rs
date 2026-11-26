//! Trellis's own checkout has no partial file (#1328), and the Rust grammar limit that
//! made one is pinned.
use super::*;

/// Trellis's own checkout has no partial file (#1328): every source an index capture
/// admits parses, with its grammar, with no syntax error, within the syntax bounds and
/// within the Rust parse-work budget, as the references read that reports
/// `partialFiles` parses it.
#[test]
fn trellis_checkout_has_no_partial_files() {
    let root = fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let cancel = CancelFlag::default();
    let (_, sources, _) = walk(
        &root,
        crate::skipped_config::CONFIG_INPUT_MAX_BYTES,
        &cancel,
    )
    .unwrap();
    assert!(
        sources.len() > 100,
        "the walk admitted {} sources",
        sources.len()
    );
    let partial = |path: &PathBuf| -> Option<String> {
        let language = language_of_path(path).unwrap();
        let name = path.strip_prefix(&root).unwrap().display();
        let Ok(text) = String::from_utf8(fs::read(path).unwrap()) else {
            return Some(format!("{name}: not_utf8"));
        };
        let grammar: tree_sitter::Language = match language {
            "java" => tree_sitter_java::LANGUAGE.into(),
            "rust" => tree_sitter_rust::LANGUAGE.into(),
            "python" => tree_sitter_python::LANGUAGE.into(),
            "javascript" => tree_sitter_javascript::LANGUAGE.into(),
            "typescript" => crate::indexer_ts::grammar(&path.to_string_lossy()),
            other => panic!("unexpected language {other}"),
        };
        let Some(tree) = crate::syntax_bounds::parse_stored(
            language,
            &grammar,
            &text,
            "stopped",
            &mut || Ok(()),
        )
        .unwrap() else {
            return Some(format!("{name}: evidence_budget"));
        };
        crate::syntax_bounds::scan(tree.root_node(), &mut || Ok(()))
            .unwrap()
            .map(|gap| format!("{name}: {gap:?}"))
    };
    let workers = std::thread::available_parallelism().map_or(4, usize::from);
    let chunk = sources.len().div_ceil(workers);
    let found: Vec<String> = std::thread::scope(|scope| {
        let partial = &partial;
        let handles: Vec<_> = sources
            .chunks(chunk)
            .map(|paths| scope.spawn(move || paths.iter().filter_map(partial).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });
    assert!(found.is_empty(), "partial files: {found:#?}");
}

/// The pinned tree-sitter-rust (0.24.2) does not parse an outer attribute on a struct
/// pattern field, which Rust accepts: the grammar limit contract-v1 records (#1328). A
/// grammar version that parses it fails this test, and the limit can then be dropped.
#[test]
fn pinned_rust_grammar_rejects_an_attribute_on_a_struct_pattern_field() {
    let parse = |text: &str| {
        let mut parser = crate::measured_parse::parser();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap().root_node().has_error()
    };
    assert!(!parse("fn f(s: S) { let S { a, b: _ } = s; }\n"));
    assert!(parse(
        "fn f(s: S) { let S { a, #[cfg(feature = \"x\")] b: _ } = s; }\n"
    ));
}
