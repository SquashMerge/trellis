use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_with_native},
    native_evidence::Artifact,
    search_capture::CapturedCard,
};

fn measure(files: &[(&str, &str)]) -> Artifact {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        fs::write(root.path().join(path), text).unwrap();
    }
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    index_workspace_with_native(
        &IndexOptions::new(root.path().into()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}
fn card<'a>(artifact: &'a Artifact, name: &str) -> &'a CapturedCard {
    let declaration = artifact
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some(name))
        .unwrap();
    artifact
        .search_cards
        .iter()
        .find(|c| c.syntax_id == declaration.syntax_id)
        .unwrap()
}
fn values(card: &CapturedCard, label: &str) -> serde_json::Value {
    let prefix = format!("{label}: ");
    let line = card
        .card
        .text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix));
    line.map(|value| serde_json::from_str(value).unwrap())
        .unwrap_or(serde_json::Value::Null)
}

#[test]
fn exact_four_language_cards_are_search_only_and_measured_once() {
    let artifact = measure(&[
        ("a.rs", "fn read() { send(\"bad_code\"); }"),
        ("a.js", "function readJs() { send(\"bad_code\"); }"),
        ("a.py", "def read_py():\n    send(\"bad_code\")\n"),
        (
            "A.java",
            "class A { void readJava() { send(\"bad_code\"); } }",
        ),
    ]);
    for (name, path, kind, signature, head, ancestors) in [
        (
            "read",
            "a.rs",
            "function",
            "function read()",
            "fn read() { send(\"bad_code\"); }",
            "[]",
        ),
        (
            "readJs",
            "a.js",
            "function",
            "function readJs()",
            "function readJs() { send(\"bad_code\"); }",
            "[]",
        ),
        (
            "read_py",
            "a.py",
            "function",
            "function read_py()",
            "def read_py():\n send(\"bad_code\")",
            "[]",
        ),
        (
            "readJava",
            "A.java",
            "method",
            "method readJava() -> void",
            "void readJava() { send(\"bad_code\"); }",
            "[{\"kind\":\"type\",\"name\":\"A\",\"selfType\":null,\"trait\":null}]",
        ),
    ] {
        let captured = card(&artifact, name);
        let type_line = if path == "A.java" {
            "types: [\"void\"]\n"
        } else {
            ""
        };
        let expected = format!(
            "kind: \"{kind}\"\nname: \"{name}\"\nancestors: {ancestors}\npath: \"{path}\"\nsignature: \"{signature}\"\ncalls: [\"send\"]\n{type_line}strings: [\"bad_code\"]\nerror codes: [\"bad_code\"]\nbody:\n{head}"
        );
        assert_eq!(captured.card.text, expected, "{path}");
        assert_eq!(
            captured.card.hash,
            trellis::search_card::card_hash(expected.as_bytes())
        );
    }
    let wire = serde_json::to_value(&artifact).unwrap();
    assert!(wire.get("searchCards").is_none());
    assert!(wire.get("search_cards").is_none());
    let restored: Artifact = serde_json::from_value(wire).unwrap();
    assert!(restored.search_cards.is_empty());
}

#[test]
fn nested_candidates_own_facts_but_head_is_contiguous() {
    let artifact = measure(&[(
        "a.py",
        "def outer():\n    \"\"\"outer docs\"\"\"\n    first(\"outer_code\")\n    def inner():\n        \"\"\"inner docs\"\"\"\n        second(\"inner_code\")\n    third(\"after_code\")\n",
    )]);
    let outer = card(&artifact, "outer");
    let inner = card(&artifact, "inner");
    assert_eq!(values(outer, "doc"), "outer docs");
    assert_eq!(values(inner, "doc"), "inner docs");
    assert_eq!(
        values(outer, "calls"),
        serde_json::json!(["first", "third"])
    );
    assert_eq!(values(inner, "calls"), serde_json::json!(["second"]));
    assert_eq!(
        values(outer, "strings"),
        serde_json::json!(["outer_code", "after_code"])
    );
    assert_eq!(values(inner, "strings"), serde_json::json!(["inner_code"]));
    assert!(outer.facts.head.contains("inner_code"));
    assert_eq!(inner.ancestors[0].name.as_deref(), Some("outer"));
}

#[test]
fn docs_literals_impl_fields_and_constructor_folding_follow_syntax() {
    let artifact = measure(&[
        (
            "a.rs",
            "struct Item;\nimpl<T> service::Reader<T> for Item {\n /// Read a value.\n #[inline]\n fn read(&self) { let value: Result<Item, Failure> = make(r###\"raw_bad\\n\"###); opaque!(\"hidden_code\"); }\n}\n",
        ),
        (
            "A.java",
            "/** Class docs. */\nclass A {\n A() { build(\"ctor_code\"); }\n /** Read docs.\n  * More.\n  */\n @Deprecated public void readJava() { log(\"java_bad\\n\"); }\n}\n",
        ),
        (
            "a.js",
            "/** JS docs. */\nfunction readJs() { return `template_${call(\"nested_code\")}`; }\n",
        ),
        (
            "a.py",
            "def read_py():\n    r\"\"\"Python \\n docs\"\"\"\n    value: Thing = f\"err_code_{call()}\"\n",
        ),
    ]);
    let rust = card(&artifact, "read");
    assert_eq!(rust.ancestors.len(), 1);
    assert_eq!(rust.ancestors[0].self_type.as_deref(), Some("Item"));
    assert_eq!(
        rust.ancestors[0].trait_name.as_deref(),
        Some("service::Reader<T>")
    );
    assert_eq!(values(rust, "doc"), "Read a value.");
    assert_eq!(values(rust, "strings"), serde_json::json!(["raw_bad\\n"]));
    assert_eq!(values(rust, "calls"), serde_json::json!(["make"]));
    assert!(rust.facts.types.contains(&"Failure".to_owned()));
    let java_type = card(&artifact, "A");
    assert_eq!(values(java_type, "doc"), "Class docs.");
    assert_eq!(values(java_type, "calls"), serde_json::json!(["build"]));
    assert_eq!(
        values(java_type, "strings"),
        serde_json::json!(["ctor_code"])
    );
    assert_eq!(
        values(java_type, "members"),
        serde_json::json!(["A", "readJava"])
    );
    assert_eq!(
        values(card(&artifact, "readJava"), "doc"),
        "Read docs. More."
    );
    let js = card(&artifact, "readJs");
    assert_eq!(values(js, "doc"), "JS docs.");
    assert_eq!(
        values(js, "strings"),
        serde_json::json!(["template_${call(\"nested_code\")}"])
    );
    assert_eq!(values(js, "calls"), serde_json::json!(["call"]));
    let py = card(&artifact, "read_py");
    assert_eq!(values(py, "doc"), "Python \\n docs");
    assert_eq!(
        values(py, "strings"),
        serde_json::json!(["err_code_{call()}"])
    );
    assert_eq!(values(py, "types"), serde_json::json!(["Thing"]));
    assert_eq!(artifact.search_cards.len(), 6); // No file-module, unnamed impl or constructor cards.
}

#[test]
fn attachment_breaks_and_scalar_caps_are_exact_and_display_independent() {
    let docs = "é".repeat(400);
    let literal = "界".repeat(50);
    let source = format!(
        "/// {docs}\nfn documented() {{ consume(\"{literal}\"); }}\n/// detached\n\nfn blank() {{}}\n/// interrupted\n// ordinary\nfn ordinary() {{}}\n"
    );
    let artifact = measure(&[("a.rs", &source)]);
    let captured = card(&artifact, "documented");
    assert_eq!(captured.facts.doc.len(), 600);
    assert_eq!(captured.facts.strings[0].len(), 99);
    assert_eq!(
        values(card(&artifact, "blank"), "doc"),
        serde_json::Value::Null
    );
    assert_eq!(
        values(card(&artifact, "ordinary"), "doc"),
        serde_json::Value::Null
    );
    // The serializer takes no display projection or lookup key.
    let declaration = artifact
        .declarations
        .iter()
        .find(|d| d.syntax_id == captured.syntax_id)
        .unwrap();
    let mut renamed_lookup = declaration.clone();
    renamed_lookup.lookup_key = Some("display-only-change".into());
    let rerendered = trellis::search_capture::render_card(&renamed_lookup, captured);
    assert_eq!(rerendered, captured.card);
}

#[test]
fn minimal_block_docs_are_empty_and_never_panic() {
    let artifact = measure(&[
        (
            "a.rs",
            "/**/\nfn empty_rust() {}\n/***/\nfn ordinary_rust() {}\n",
        ),
        (
            "a.js",
            "/**/\nfunction emptyJs() {}\n/***/\nfunction ordinaryJs() {}\n",
        ),
        (
            "A.java",
            "class A {\n/**/\nvoid emptyJava() {}\n/***/\nvoid ordinaryJava() {}\n}\n",
        ),
        ("a.py", "def empty_py():\n    \"\"\"\"\"\"\n    pass\n"),
    ]);
    for name in [
        "empty_rust",
        "ordinary_rust",
        "emptyJs",
        "ordinaryJs",
        "emptyJava",
        "ordinaryJava",
        "empty_py",
    ] {
        assert_eq!(
            values(card(&artifact, name), "doc"),
            serde_json::Value::Null,
            "{name}"
        );
    }
}

#[test]
fn rust_consumed_line_endings_do_not_attach_docs_across_blank_lines() {
    let artifact = measure(&[(
        "a.rs",
        "/// first\n/// second\nfn consecutive() {}\n/// detached\n\n#[inline]\nfn attributed() {}\nmod owner {\n//! inner first\n\n//! inner detached\nfn nested() {}\n}\n",
    )]);
    assert_eq!(
        values(card(&artifact, "consecutive"), "doc"),
        "first second"
    );
    assert_eq!(
        values(card(&artifact, "attributed"), "doc"),
        serde_json::Value::Null
    );
    assert_eq!(values(card(&artifact, "owner"), "doc"), "inner first");
    assert_eq!(
        values(card(&artifact, "nested"), "doc"),
        serde_json::Value::Null
    );
}

#[test]
fn python_docstring_is_selected_once_and_other_string_statements_stay_literals() {
    let statements: String = (0..30).map(|i| format!("    \"value_{i}\"\n")).collect();
    let source = format!(
        "def strings():\n    # initial comment\n    \"\"\"selected docs\"\"\"\n{statements}"
    );
    let artifact = measure(&[("a.py", &source)]);
    let captured = card(&artifact, "strings");
    assert_eq!(captured.facts.doc, "selected docs");
    assert_eq!(
        captured.facts.strings,
        (0..20).map(|i| format!("value_{i}")).collect::<Vec<_>>()
    );
}

#[test]
fn rust_raw_quote_prefixes_are_payload_not_triple_delimiters() {
    let artifact = measure(&[(
        "a.rs",
        r####"fn raw_quotes() { consume(r#"""abc"#, r##""""abc"##, r#"quote_\"code"#); }"####,
    )]);
    assert_eq!(
        values(card(&artifact, "raw_quotes"), "strings"),
        serde_json::json!(["\"\"abc", "\"\"\"abc", "quote_\\\"code"])
    );
}
