#![cfg(feature = "test-support")]
use crate::common;
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    files_view::SyntaxMatchesRequest,
    indexer::{IndexOptions, index_workspace_bundle},
    model::{Graph, IndexPin},
    store::{Store, topology::WriterSession},
};

fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, Store, Graph, Arc<WriterSession>) {
    fixture_with_file_limit(files, None)
}
fn fixture_with_file_limit(
    files: &[(&str, &str)],
    max_file_bytes: Option<u64>,
) -> (tempfile::TempDir, Store, Graph, Arc<WriterSession>) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (path, text) in files {
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let store = common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut options = IndexOptions::new(workspace);
    if let Some(max_file_bytes) = max_file_bytes {
        options.max_file_bytes = max_file_bytes;
    }
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    (temp, store, graph, session)
}
fn request(pin: IndexPin, id: &str, limit: usize, continuation: Value) -> SyntaxMatchesRequest {
    serde_json::from_value(
        json!({"expectedRevision":pin,"symbolId":id,"limit":limit,"continuation":continuation}),
    )
    .unwrap()
}

#[test]
fn outline_keeps_every_measured_kind_native_header_identity_and_pages_on_cached_pin() {
    let text = "class Box<T> { int field; Box(int n) { field = n; } int call(int p) { int local = p; return local; } }\n";
    let (temp, store, graph, _session) =
        fixture(&[("Box.java", text), ("Empty.java", "// no declarations\n")]);
    let pin = store.status().unwrap().revision;
    let mut items = vec![];
    let mut offset = 0;
    loop {
        let page = store
            .file_symbols_at("Box.java", pin, offset, 2)
            .unwrap()
            .unwrap();
        assert!(page.items.len() <= 2);
        assert_eq!(page.truncated, page.next_offset.is_some());
        for item in page.items {
            assert_eq!(
                graph.nodes.iter().find(|n| n.id == item.symbol.id),
                Some(&item.symbol)
            );
            if let Some(name) = item.header.name.as_deref() {
                assert_eq!(name, item.symbol.name);
            } else {
                assert_eq!(item.native_kind, "module");
            }
            items.push(item);
        }
        let Some(next) = page.next_offset else {
            break;
        };
        offset = next;
    }
    assert_eq!(
        items.len(),
        graph.nodes.iter().filter(|n| n.path == "Box.java").count()
    );
    for kind in [
        "type",
        "field",
        "constructor",
        "method",
        "parameter",
        "variable",
        "module",
    ] {
        assert!(
            items.iter().any(|i| i.native_kind == kind),
            "{kind}: {items:?}"
        );
    }
    assert!(
        items
            .iter()
            .any(|i| i.native_kind == "method" && i.header.result_type.as_deref() == Some("int"))
    );
    let empty = store
        .file_symbols_at("Empty.java", pin, 0, 200)
        .unwrap()
        .unwrap();
    assert!(empty.items.iter().all(|i| i.native_kind == "module"));
    let module = empty.items.first().unwrap();
    let unavailable = store
        .syntax_matches_at(&request(pin, &module.symbol.id, 50, Value::Null))
        .unwrap()
        .unwrap();
    assert!(unavailable.partial && unavailable.scope.spelling.is_none());
    assert!(unavailable.items.is_empty() && unavailable.continuation.is_none());
    assert!(
        store
            .file_symbols_at("missing.java", pin, 0, 200)
            .unwrap()
            .is_none()
    );
    let before =
        serde_json::to_value(store.file_symbols_at("Box.java", pin, 0, 200).unwrap()).unwrap();
    std::fs::remove_file(temp.path().join("workspace/Box.java")).unwrap();
    assert_eq!(
        before,
        serde_json::to_value(store.file_symbols_at("Box.java", pin, 0, 200).unwrap()).unwrap()
    );
    assert_eq!(
        store
            .source_at("Box.java", Some(pin))
            .unwrap()
            .unwrap()
            .1
            .text,
        text
    );
    let catalog = store.files_at(Some(pin), 0, 200).unwrap();
    assert!(
        catalog["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["path"] == "Box.java")
    );
}

#[test]
fn outline_retains_native_alias_and_implementation_without_callable_coercion() {
    let (_temp, store, graph, _session) = fixture(&[(
        "a.rs",
        "struct Box { field: i32 }\ntype Alias = Box;\nimpl Box { fn call(&self) -> i32 { self.field } }\n",
    )]);
    let pin = store.status().unwrap().revision;
    let page = store.file_symbols_at("a.rs", pin, 0, 200).unwrap().unwrap();
    for kind in ["type", "alias", "implementation", "field", "method"] {
        let item = page.items.iter().find(|i| i.native_kind == kind).unwrap();
        assert_eq!(
            graph.nodes.iter().find(|n| n.id == item.symbol.id),
            Some(&item.symbol)
        );
    }
}

#[test]
fn syntax_matches_cached_utf8_paging_scope_and_spelling_are_not_bindings() {
    let a = "function chosen(p) { return p; }\n// chosen ignored\nconst literal = 'chosen';\nfunction other() { const é = 1; chosen(é); chosen(2); longerchosen(); }\nconst template = `chosen ${chosen(3)}`;\n";
    let b = "function homonym() { function chosen() {} chosen(); }\n";
    let (temp, store, graph, _session) = fixture(&[
        ("a.js", a),
        ("b.js", b),
        ("c.py", "def chosen(): pass\nchosen()\n"),
    ]);
    let pin = store.status().unwrap().revision;
    let seed = graph
        .nodes
        .iter()
        .find(|s| s.path == "a.js" && s.name == "chosen")
        .unwrap();
    let mut cursor = Value::Null;
    let mut locations = vec![];
    loop {
        let result = store
            .syntax_matches_at(&request(pin, &seed.id, 1, cursor))
            .unwrap()
            .unwrap();
        assert_eq!(result.evidence_kind, "syntaxMatches");
        assert_eq!(result.scope.language, "javascript");
        assert_eq!(
            result.scope.source_set_id,
            format!("source-set:v1:{}", store.root_id())
        );
        assert_eq!(result.scope.spelling.as_deref(), Some("chosen"));
        assert_eq!(result.truncated, result.continuation.is_some());
        assert!(!result.partial, "{:?}", result.warnings);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("not compiler-resolved"))
        );
        for item in &result.items {
            let source = if item.path == "a.js" { a } else { b };
            assert_eq!(
                &source[item.range.start_byte..item.range.end_byte],
                "chosen"
            );
            assert_eq!(
                item.range.start_column,
                source[..item.range.start_byte]
                    .rsplit('\n')
                    .next()
                    .unwrap()
                    .len()
                    + 1
            );
            assert!(item.snippet.contains("chosen"));
            locations.push((item.path.clone(), item.range.start_byte));
        }
        match result.continuation {
            Some(c) => cursor = serde_json::to_value(c).unwrap(),
            None => break,
        }
    }
    assert_eq!(locations.len(), 5, "{locations:?}");
    assert!(locations.windows(2).all(|w| w[0] < w[1]));
    let before = serde_json::to_value(
        store
            .syntax_matches_at(&request(pin, &seed.id, 200, Value::Null))
            .unwrap(),
    )
    .unwrap();
    std::fs::write(
        temp.path().join("workspace/a.js"),
        "function replacement() {}\n",
    )
    .unwrap();
    std::fs::remove_file(temp.path().join("workspace/b.js")).unwrap();
    assert_eq!(
        before,
        serde_json::to_value(
            store
                .syntax_matches_at(&request(pin, &seed.id, 200, Value::Null))
                .unwrap()
        )
        .unwrap()
    );
    let first = store
        .syntax_matches_at(&request(pin, &seed.id, 1, Value::Null))
        .unwrap()
        .unwrap();
    let cursor = serde_json::to_value(first.continuation.unwrap()).unwrap();
    let mut wrong = cursor.clone();
    wrong["symbolId"] = json!("other");
    assert!(request(pin, &seed.id, 1, wrong).validate().is_err());
    let mut wrong = cursor;
    wrong["revision"]["indexGeneration"] = json!(uuid::Uuid::new_v4().to_string());
    assert!(request(pin, &seed.id, 1, wrong).validate().is_err());
    let wrong_pin = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        ..pin
    };
    assert!(
        store
            .syntax_matches_at(&request(wrong_pin, &seed.id, 1, Value::Null))
            .unwrap_err()
            .to_string()
            .contains("revision conflict")
    );
}

#[test]
fn syntax_leaf_dispatch_handles_java_rust_python_and_interpolation() {
    for (path, language, text, expected) in [
        (
            "a.rs",
            "rust",
            "fn chosen() {}\nfn use_it() { chosen(); let s = \"chosen\"; /* chosen */ }\n",
            1,
        ),
        (
            "A.java",
            "java",
            "class A { int chosen; int useIt() { return chosen; } String s = \"chosen\"; /* chosen */ }\n",
            1,
        ),
        (
            "a.py",
            "python",
            "def chosen(): pass\nchosen()\nx = 'chosen' # chosen\ny = f'chosen {chosen()}'\n",
            2,
        ),
    ] {
        let (_temp, store, graph, _session) = fixture(&[(path, text)]);
        let pin = store.status().unwrap().revision;
        let seed = graph.nodes.iter().find(|n| n.name == "chosen").unwrap();
        let result = store
            .syntax_matches_at(&request(pin, &seed.id, 200, Value::Null))
            .unwrap()
            .unwrap();
        assert_eq!(result.scope.language, language);
        assert_eq!(result.items.len(), expected, "{path}: {result:?}");
    }
}

#[test]
fn rust_destructuring_shorthand_is_a_same_spelling_syntax_match() {
    let text = "struct S { chosen: i32 }\nfn destructure(s: S) { let S { chosen } = s; }\n";
    let (_temp, store, graph, _session) = fixture(&[("a.rs", text)]);
    let pin = store.status().unwrap().revision;
    let field = graph.nodes.iter().find(|n| n.name == "chosen").unwrap();
    let result = store
        .syntax_matches_at(&request(pin, &field.id, 200, Value::Null))
        .unwrap()
        .unwrap();
    assert!(!result.partial);
    assert!(!result.truncated);
    assert_eq!(result.items.len(), 1);
    let occurrence = &result.items[0];
    assert_eq!(occurrence.range.start_byte, text.rfind("chosen").unwrap());
    assert_eq!(
        &text[occurrence.range.start_byte..occurrence.range.end_byte],
        "chosen"
    );
    assert_eq!(result.evidence_kind, "syntaxMatches");
}

#[test]
fn over_budget_file_is_skipped_honestly_and_cursor_retains_partial_coverage() {
    let mut large = String::from("// ");
    large.push_str(&"x".repeat(2 * 1024 * 1024));
    // The index capture's default is also 2 MiB. Admit this valid cached
    // file explicitly above that default to exercise the narrower query cap.
    let (_temp, store, graph, _session) = fixture_with_file_limit(
        &[
            ("a.js", "function chosen() {}\n"),
            ("b.js", &large),
            ("c.js", "chosen(); chosen();\n"),
        ],
        Some(large.len() as u64),
    );
    let pin = store.status().unwrap().revision;
    let seed = graph.nodes.iter().find(|n| n.name == "chosen").unwrap();
    let first = store
        .syntax_matches_at(&request(pin, &seed.id, 1, Value::Null))
        .unwrap()
        .unwrap();
    assert!(first.partial && first.truncated);
    assert!(first.warnings.iter().any(|w| w.contains("2 MiB")));
    let cursor = first.continuation.unwrap();
    assert!(cursor.partial);
    let next = store
        .syntax_matches_at(&request(
            pin,
            &seed.id,
            1,
            serde_json::to_value(cursor).unwrap(),
        ))
        .unwrap()
        .unwrap();
    assert!(next.partial);
    assert!(!next.truncated);
    assert_eq!(next.items.len(), 1);
}

#[test]
fn malformed_cached_source_refuses_whole_read_not_partial_matches() {
    let (temp, store, graph, _session) = fixture(&[
        ("a.js", "function chosen() {}\nchosen();\n"),
        ("b.js", "chosen();\n"),
    ]);
    let pin = store.status().unwrap().revision;
    let seed = graph.nodes.iter().find(|n| n.name == "chosen").unwrap();
    let db_path = std::fs::read_dir(temp.path().join("state/cache/indexes"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap()
        .join("index.db");
    // Keep this independent connection open until after the managed store's reads.
    let db = rusqlite::Connection::open(db_path).unwrap();
    db.execute(
        "UPDATE document_versions SET source_bytes=X'FF',byte_length=1 WHERE path='b.js'",
        [],
    )
    .unwrap();
    let error = store
        .syntax_matches_at(&request(pin, &seed.id, 200, Value::Null))
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("incompatible_index"),
        "{error:#}"
    );
    assert!(store.recovery_pending_for_tests());
}
