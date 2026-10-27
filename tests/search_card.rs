use trellis::native_evidence::{Header, Parameter};
use trellis::search_card::{
    Ancestor, CALL_COUNT, CardFormat, CardInput, DOC_BYTES, HEAD_SOURCE_BYTES, LITERAL_BYTES,
    LITERAL_COUNT, MeasuredFacts, PreparedFacts, TYPE_COUNT, card_hash, error_codes, is_eligible,
    prefix, prepare_facts, prepare_head, render_signature, serialize, space,
};

fn header(kind: &str, name: &str) -> Header {
    Header {
        kind: kind.into(),
        name: Some(name.into()),
        modifiers: Vec::new(),
        type_parameters: Vec::new(),
        parameters: Vec::new(),
        result_type: None,
        bases: Vec::new(),
    }
}

#[test]
fn golden_card_has_exact_labels_order_canonical_json_and_literal_head() {
    let mut header = header("method", "go\n\"\\雪");
    header.modifiers = vec![" public\t".into()];
    header.parameters = vec![Parameter {
        name: Some("arg".into()),
        type_name: Some(" Result\n< T > ".into()),
        variadic: true,
    }];
    header.result_type = Some(" T ".into());
    let ancestors = vec![
        Ancestor {
            kind: "type".into(),
            name: Some("Outer".into()),
            self_type: None,
            trait_name: None,
        },
        Ancestor {
            kind: "implementation".into(),
            name: None,
            self_type: Some("Widget<T>".into()),
            trait_name: Some("Run".into()),
        },
        Ancestor {
            kind: "type".into(),
            name: Some("Outer".into()),
            self_type: None,
            trait_name: None,
        },
    ];
    let facts = PreparedFacts {
        doc: "A \"doc\" \\ \u{0008}\u{000c}\u{0000} 雪".into(),
        calls: vec!["helper".into()],
        types: vec!["Result < T >".into(), "T".into()],
        strings: vec!["bad_code\nbody:\t\\\"".into()],
        error_codes: vec!["bad_code".into()],
        members: Vec::new(),
        head: " {\n body:\t雪\r\n}".into(),
    };
    let card = serialize(&CardInput {
        kind: "method",
        name: "go\n\"\\雪",
        ancestors: &ancestors,
        path: "src/雪.java",
        header: &header,
        facts: &facts,
    });
    let expected = concat!(
        "kind: \"method\"\n",
        "name: \"go\\u000a\\\"\\\\雪\"\n",
        "ancestors: [{\"kind\":\"type\",\"name\":\"Outer\",\"selfType\":null,\"trait\":null},{\"kind\":\"implementation\",\"name\":null,\"selfType\":\"Widget<T>\",\"trait\":\"Run\"},{\"kind\":\"type\",\"name\":\"Outer\",\"selfType\":null,\"trait\":null}]\n",
        "path: \"src/雪.java\"\n",
        "signature: \"public method go \\\"\\\\雪(...arg: Result < T >) -> T\"\n",
        "doc: \"A \\\"doc\\\" \\\\ \\u0008\\u000c\\u0000 雪\"\n",
        "calls: [\"helper\"]\n",
        "types: [\"Result < T >\",\"T\"]\n",
        "strings: [\"bad_code\\u000abody:\\u0009\\\\\\\"\"]\n",
        "error codes: [\"bad_code\"]\n",
        "body:\n",
        " {\n body:\t雪\r\n}"
    );
    assert_eq!(card.text.as_bytes(), expected.as_bytes());
    assert_eq!(card.hash, card_hash(expected.as_bytes()));
    assert!(!card.text.ends_with('\n'));
}

#[test]
fn mandatory_lines_empty_body_and_hash_domain_are_fixed() {
    let header = header("function", "go");
    let facts = PreparedFacts::default();
    let card = serialize(&CardInput {
        kind: "function",
        name: "go",
        ancestors: &[],
        path: "src/a.rs",
        header: &header,
        facts: &facts,
    });
    assert_eq!(
        card.text,
        "kind: \"function\"\nname: \"go\"\nancestors: []\npath: \"src/a.rs\"\nsignature: \"function go()\"\nbody:\n"
    );
    assert_eq!(CardFormat::V1.as_str(), "trellis.search-card.v1");
    assert_eq!(
        card.hash,
        "99d175b8af7cad32cd39b1f56d16357384bd622615930a1fe0469ff5581f1929"
    );
    let mut newline_facts = facts;
    newline_facts.head = "source\n".into();
    let with_newline = serialize(&CardInput {
        kind: "function",
        name: "go",
        ancestors: &[],
        path: "src/a.rs",
        header: &header,
        facts: &newline_facts,
    });
    assert!(with_newline.text.ends_with("\nbody:\nsource\n"));
    assert!(!with_newline.text.ends_with("source\n\n"));
    assert_ne!(card.hash, with_newline.hash);
}

#[test]
fn preparation_respects_scalar_boundaries_and_only_ascii_whitespace() {
    assert_eq!(space(" \t\r\nA \t B\r\n C \t"), "A B C");
    assert_eq!(
        space("\u{a0}A\u{b}B\u{c}C\u{2003}"),
        "\u{a0}A\u{b}B\u{c}C\u{2003}"
    );
    for (text, cap, expected) in [("a雪z", 3, "a"), ("a雪z", 4, "a雪"), ("😀", 3, "")] {
        assert_eq!(prefix(text, cap), expected);
    }
    let header = header("function", "f");
    let doc = format!("{}雪tail", "a".repeat(DOC_BYTES - 1));
    let strings = vec![format!("{}😀tail", "b".repeat(LITERAL_BYTES - 1))];
    let head = format!("{}雪tail", "c".repeat(HEAD_SOURCE_BYTES - 1));
    let facts = prepare_facts(
        "function",
        &header,
        MeasuredFacts {
            doc: &doc,
            strings: &strings,
            head: &head,
            ..MeasuredFacts::default()
        },
    );
    assert_eq!(facts.doc, "a".repeat(599));
    assert_eq!(facts.strings, ["b".repeat(99)]);
    assert_eq!(facts.head, "c".repeat(1199));
    let spaces = format!("{}MUST_NOT_SURVIVE", " ".repeat(1200));
    assert_eq!(prepare_head(&spaces), " ");
    assert_eq!(
        prepare_head(" \t A\t\t\r\n  B\rC\n\u{a0} "),
        " A \r\n B\rC\n\u{a0} "
    );
}

#[test]
fn contiguous_head_keeps_nested_declarations_and_is_not_trimmed() {
    let source = "class Outer {\n\tvoid inner() { call(); }\n}\n";
    let header = header("type", "Outer");
    let facts = prepare_facts(
        "type",
        &header,
        MeasuredFacts {
            head: source,
            ..MeasuredFacts::default()
        },
    );
    let card = serialize(&CardInput {
        kind: "type",
        name: "Outer",
        ancestors: &[],
        path: "Outer.java",
        header: &header,
        facts: &facts,
    });
    assert!(
        card.text
            .ends_with("\nbody:\nclass Outer {\n void inner() { call(); }\n}\n")
    );
}

#[test]
fn fact_preparation_deduplicates_after_preparation_then_applies_caps() {
    let mut header = header("type", "A");
    header.parameters = vec![
        Parameter {
            name: None,
            type_name: Some("\tFirst\n Type ".into()),
            variadic: false,
        },
        Parameter {
            name: None,
            type_name: Some("First Type".into()),
            variadic: false,
        },
    ];
    header.result_type = Some(" Result ".into());
    let mut body_types = vec!["Result".into(), "First\tType".into(), "".into()];
    body_types.extend((0..35).map(|index| format!("Body{index}")));
    let mut calls = vec!["".into(), " call\n one ".into(), "call one".into()];
    calls.extend((0..45).map(|index| format!("call{index}")));
    let mut strings = vec!["".into(), " ".into(), " ".into()];
    let common_prefix = "x".repeat(100);
    strings.extend([format!("{common_prefix}a"), format!("{common_prefix}b")]);
    strings.extend((0..25).map(|index| format!("literal_{index}")));
    let members = vec!["first".into(), "".into(), "first".into(), " second ".into()];
    let measured = MeasuredFacts {
        doc: " First\r\nSecond ",
        calls: &calls,
        body_types: &body_types,
        strings: &strings,
        members: &members,
        head: "",
    };
    let facts = prepare_facts("type", &header, measured);
    assert_eq!(facts.doc, "First Second");
    assert_eq!(facts.calls.len(), CALL_COUNT);
    assert_eq!(&facts.calls[..3], ["call one", "call0", "call1"]);
    assert_eq!(facts.calls.last().unwrap(), "call38");
    assert_eq!(facts.types.len(), TYPE_COUNT);
    assert_eq!(&facts.types[..3], ["First Type", "Result", "Body0"]);
    assert_eq!(facts.types.last().unwrap(), "Body27");
    assert_eq!(facts.strings.len(), LITERAL_COUNT);
    assert_eq!(facts.strings[0], " ");
    assert_eq!(facts.strings[1], common_prefix);
    assert_eq!(facts.strings.last().unwrap(), "literal_17");
    assert_eq!(facts.error_codes.last().unwrap(), "literal_17");
    assert!(!facts.error_codes.iter().any(|value| value == "literal_18"));
    assert_eq!(facts.members, ["first", " second "]);
    assert!(
        prepare_facts("function", &header, measured)
            .members
            .is_empty()
    );
}

#[test]
fn error_tokens_require_the_entire_maximal_ascii_run_and_have_no_extra_cap() {
    let strings = vec![
        "ok_code ok_code a1_b2 a_2_3 bad__code _bad_code bad_code_ BAD_code a_B 1_bad a-b".into(),
        "éunicode_code😀next_code good_codeX good_code9!slash_code\\nraw_code".into(),
    ];
    assert_eq!(
        error_codes(&strings),
        [
            "ok_code",
            "a1_b2",
            "a_2_3",
            "unicode_code",
            "next_code",
            "good_code9",
            "slash_code",
            "nraw_code"
        ]
    );
    let header = header("type", "Many");
    let strings = vec![
        (0..20)
            .map(|index| format!("error_{index}"))
            .collect::<Vec<_>>()
            .join(" "),
    ];
    // The long payload is truncated first; a partial final run is still measured.
    let facts = prepare_facts(
        "type",
        &header,
        MeasuredFacts {
            strings: &strings,
            ..MeasuredFacts::default()
        },
    );
    assert_eq!(facts.strings[0], prefix(&strings[0], 100));
    assert_eq!(facts.error_codes.last().unwrap(), "error_11");
    let strings = (0..20)
        .map(|index| format!("first_{index} second_{index}"))
        .collect::<Vec<_>>();
    let members = (0..45)
        .map(|index| format!("member{index}"))
        .collect::<Vec<_>>();
    let facts = prepare_facts(
        "type",
        &header,
        MeasuredFacts {
            strings: &strings,
            members: &members,
            ..MeasuredFacts::default()
        },
    );
    assert_eq!(facts.error_codes.len(), 40);
    assert_eq!(facts.members.len(), 45);
}

#[test]
fn signature_uses_measured_components_preserving_structural_arrays() {
    let mut header = header("method", " run ");
    header.modifiers = vec![" public\n".into(), "public".into(), " ".into()];
    header.type_parameters = vec!["<T>".into(), "<T>".into(), "".into()];
    header.parameters = vec![
        Parameter {
            name: None,
            type_name: Some(" Vec < T >\n".into()),
            variadic: true,
        },
        Parameter {
            name: Some(" x ".into()),
            type_name: None,
            variadic: false,
        },
    ];
    header.result_type = Some("\tT\n".into());
    header.bases = vec!["Base".into(), "Base".into(), "".into()];
    assert_eq!(
        render_signature(&header),
        "public public method run<<T>, <T>, >(..._: Vec < T >, x) -> T : Base, Base, "
    );
    for kind in ["type", "implementation", "namespace", "module"] {
        let mut container = self::header(kind, "C");
        assert_eq!(render_signature(&container), format!("{kind} C"));
        container.parameters.push(Parameter {
            name: Some("x".into()),
            type_name: None,
            variadic: false,
        });
        assert_eq!(render_signature(&container), format!("{kind} C(x)"));
    }
}

#[test]
fn eligibility_is_closed_and_requires_a_measured_nonempty_name() {
    for kind in [
        "function",
        "method",
        "type",
        "implementation",
        "namespace",
        "module",
    ] {
        assert!(is_eligible(kind, Some("name")));
        assert!(!is_eligible(kind, None));
        assert!(!is_eligible(kind, Some("")));
    }
    for kind in [
        "constructor",
        "field",
        "variable",
        "parameter",
        "alias",
        "anonymousFunction",
        "Function",
        "",
    ] {
        assert!(!is_eligible(kind, Some("name")));
    }
}

#[test]
fn container_members_follow_error_codes_and_measured_names_are_not_folded() {
    let header = header("type", " A ");
    let members = vec![" field\tname ".into()];
    let strings = vec!["bad_code".into()];
    let facts = prepare_facts(
        "type",
        &header,
        MeasuredFacts {
            strings: &strings,
            members: &members,
            ..MeasuredFacts::default()
        },
    );
    let card = serialize(&CardInput {
        kind: "type",
        name: " A ",
        ancestors: &[],
        path: "dir with space/A.java",
        header: &header,
        facts: &facts,
    });
    assert_eq!(
        card.text,
        concat!(
            "kind: \"type\"\n",
            "name: \" A \"\n",
            "ancestors: []\n",
            "path: \"dir with space/A.java\"\n",
            "signature: \"type A\"\n",
            "strings: [\"bad_code\"]\n",
            "error codes: [\"bad_code\"]\n",
            "members: [\" field\\u0009name \"]\n",
            "body:\n"
        )
    );
}
