use serde_json::json;
use tree_sitter::{Language, Parser};

struct Case {
    language: &'static str,
    extension: &'static str,
    kind: &'static str,
    source: &'static str,
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for (language, extension, comment, normal) in [
        (
            "javascript",
            "js",
            "  // comment\n",
            "  function normal() {}\n",
        ),
        ("rust", "rs", "  // comment\n", "  fn normal() {}\n"),
        ("java", "java", "  // comment\n", "  class Normal {}\n"),
        ("python", "py", "  # comment\n", "def normal():\n    pass\n"),
    ] {
        for (kind, source) in [
            ("lf", "\n\n"),
            ("spaces", "   "),
            ("crlf", "\r\n\r\n"),
            ("empty", ""),
            ("comment", comment),
            ("normal", normal),
        ] {
            cases.push(Case {
                language,
                extension,
                kind,
                source,
            });
        }
    }
    cases
}

fn grammar(language: &str) -> Language {
    match language {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => unreachable!(),
    }
}

// Measured independently with the pinned grammars on the unchanged baseline.
fn expected_root_range(case: &Case) -> (usize, usize) {
    match (case.language, case.kind) {
        (_, "lf") => (2, 2),
        (_, "spaces") => (3, 3),
        (_, "crlf") => (4, 4),
        (_, "empty") => (0, 0),
        ("javascript" | "rust" | "java", "comment") => (2, 13),
        ("python", "comment") => (2, 12),
        ("javascript", "normal") => (2, 23),
        ("rust", "normal") => (2, 17),
        ("java", "normal") => (2, 18),
        ("python", "normal") => (0, 23),
        _ => unreachable!(),
    }
}

#[cfg(feature = "test-support")]
fn expected_module_range(case: &Case) -> (usize, usize) {
    let (start, end) = expected_root_range(case);
    if !case.source.is_empty() && start == end {
        (0, case.source.len())
    } else {
        (start, end)
    }
}

#[test]
fn pinned_grammar_root_ranges_are_characterized() {
    let mut observed = Vec::new();
    for case in cases() {
        let mut parser = Parser::new();
        parser.set_language(&grammar(case.language)).unwrap();
        let tree = parser.parse(case.source, None).unwrap();
        let root = tree.root_node();
        assert!(!root.has_error(), "{}/{}", case.language, case.kind);
        assert!(root.start_byte() <= root.end_byte());
        assert!(root.end_byte() <= case.source.len());
        assert_eq!(
            (root.start_byte(), root.end_byte()),
            expected_root_range(&case),
            "{}/{}",
            case.language,
            case.kind
        );
        observed.push(json!({
            "language": case.language,
            "case": case.kind,
            "extension": case.extension,
            "length": case.source.len(),
            "root": [root.start_byte(), root.end_byte()],
        }));
    }
    println!("{}", serde_json::to_string(&observed).unwrap());
}

#[cfg(feature = "test-support")]
mod publication {
    use super::{Case, cases};
    use rusqlite::{Connection, OpenFlags};
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };
    use trellis::{
        indexer::{IndexOptions, index_workspace_bundle},
        model::{CancelFlag, SymbolKind},
        native_evidence::{DeclarationRole, Range},
        store::{Store, topology::WorkspaceIdentity},
    };

    fn test_path(case: &Case) -> bool {
        let whitespace = matches!(case.kind, "lf" | "spaces" | "crlf");
        matches!(case.language, "javascript" | "python") == whitespace
    }

    fn path(case: &Case) -> String {
        format!(
            "{}/{}/{}.{}",
            if test_path(case) { "tests" } else { "src" },
            case.language,
            case.kind,
            case.extension
        )
    }

    fn assert_range(range: &Range, source: &str, empty_module: bool) {
        assert!(range.start <= range.end);
        assert!(range.end <= source.len());
        assert!(empty_module || range.start < range.end);
        assert!(source.is_char_boundary(range.start));
        assert!(source.is_char_boundary(range.end));
    }

    #[test]
    fn whitespace_modules_publish_with_strict_ranges_and_file_roles() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cases = cases();
        for case in &cases {
            let file = root.path().join(path(case));
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, case.source).unwrap();
        }
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        store.verify_root().unwrap();
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let baseline = store.index_baseline().unwrap();
        let leader = store.publication_for_tests().unwrap();
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(root.path().to_owned()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .expect("all four grammars must admit whitespace-only native modules");
        assert_eq!(native.revision.documents.len(), cases.len());
        assert_eq!(graph.files.len(), cases.len());
        let pin = store
            .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
            .unwrap();
        assert_eq!(store.status().unwrap().revision, pin);

        for case in &cases {
            let path = path(case);
            let document = native
                .revision
                .documents
                .iter()
                .find(|document| document.key.path == path)
                .unwrap();
            let declarations: Vec<_> = native
                .declarations
                .iter()
                .filter(|declaration| declaration.document == document.key)
                .collect();
            let modules: Vec<_> = declarations
                .iter()
                .filter(|declaration| declaration.kind == "module")
                .collect();
            assert_eq!(modules.len(), 1, "{path}");
            let module = modules[0];
            assert!(module.ancestors.is_empty(), "{path}");
            let expected = super::expected_module_range(case);
            assert_eq!((module.range.start, module.range.end), expected, "{path}");
            let role = if test_path(case) {
                DeclarationRole::Test
            } else {
                DeclarationRole::Production
            };
            for declaration in &declarations {
                assert_eq!(declaration.role, role, "{path}");
                assert_range(
                    &declaration.range,
                    case.source,
                    declaration.kind == "module" && case.source.is_empty(),
                );
                if let Some(name) = &declaration.name_range {
                    assert_range(name, case.source, false);
                    assert!(name.start >= declaration.range.start);
                    assert!(name.end <= declaration.range.end);
                }
                if let Some(lookup) = &declaration.lookup_key {
                    let selected = store
                        .native_declarations_at(pin, case.language, lookup)
                        .unwrap();
                    assert!(selected.contains(declaration), "{path}");
                }
            }

            let (stored, bytes) = store.native_source_at(pin, &document.key).unwrap().unwrap();
            assert_eq!(stored, *document);
            assert_eq!(bytes, case.source.as_bytes(), "{path}");
            let (selected_pin, source) = store.source_at(&path, Some(pin)).unwrap().unwrap();
            assert_eq!(selected_pin, pin);
            assert_eq!(source.text, case.source, "{path}");
            let outline = store.file_symbols_at(&path, pin, 0, 20).unwrap().unwrap();
            assert_eq!(outline.revision, pin);
            assert_eq!(outline.items.len(), declarations.len(), "{path}");
            let selected_modules: Vec<_> = outline
                .items
                .iter()
                .filter(|item| matches!(item.symbol.kind, SymbolKind::Module))
                .collect();
            assert_eq!(selected_modules.len(), 1, "{path}");
            let selected_module = &selected_modules[0].symbol;
            assert_eq!(selected_module.id, module.syntax_id);
            assert_eq!(
                (
                    selected_module.range.start_byte,
                    selected_module.range.end_byte
                ),
                expected,
                "{path}"
            );
        }

        // Modules have no lookup key. Inspect their published native rows directly,
        // scoped to the selected revision, to check their authoritative file role.
        let identity = WorkspaceIdentity::discover(Some(root.path()), root.path()).unwrap();
        let db = Connection::open_with_flags(
            state
                .path()
                .join("cache/indexes")
                .join(identity.root_key)
                .join("index.db"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for case in &cases {
            let path = path(case);
            let rows: Vec<(String, i64, i64)> = db
                .prepare(
                    "SELECT d.role,d.start_byte,d.end_byte
                     FROM native_version_declarations d
                     JOIN revision_documents r ON r.vid=d.vid
                     JOIN native_revisions n ON n.id=r.revision_id
                     JOIN index_metadata m ON m.index_revision=n.published_index_revision
                     WHERE r.path=?1 AND d.kind='module' AND d.owner_syntax_id IS NULL",
                )
                .unwrap()
                .query_map([&path], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            let expected = super::expected_module_range(case);
            assert_eq!(
                rows,
                vec![(
                    if test_path(case) {
                        "test"
                    } else {
                        "production"
                    }
                    .into(),
                    i64::try_from(expected.0).unwrap(),
                    i64::try_from(expected.1).unwrap(),
                )],
                "{path}"
            );
        }
    }
}
