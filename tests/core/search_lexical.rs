use rusqlite::Connection;
use trellis::search_lexical::{card_terms, query_expression, terms};

#[test]
fn whole_runs_precede_camel_acronym_and_letter_number_parts() {
    let connection = Connection::open_in_memory().unwrap();
    for (input, expected) in [
        ("fooBar", vec!["foobar", "foo", "bar"]),
        ("HTTPServer2", vec!["httpserver2", "http", "server"]),
        ("fooBARBaz", vec!["foobarbaz", "foo", "bar", "baz"]),
        (
            "XMLHTTPRequest",
            vec!["xmlhttprequest", "xmlhttp", "request"],
        ),
        ("sha256Sum", vec!["sha256sum", "sha", "256", "sum"]),
        ("v2Parser", vec!["v2parser", "parser"]),
        ("process_foo", vec!["process", "foo"]),
    ] {
        assert_eq!(terms(&connection, input).unwrap(), expected, "{input}");
    }
}

#[test]
fn sqlite_owns_unicode_spans_and_fold_without_early_diacritic_or_porter_changes() {
    let connection = Connection::open_in_memory().unwrap();
    for (input, expected) in [
        ("écoleHTTP", vec!["écolehttp", "école", "http"]),
        ("ab１２Cd", vec!["ab１２cd", "ab", "１２", "cd"]),
        ("ab中12Cd", vec!["ab中12cd", "ab中", "12", "cd"]),
        ("ab\u{e000}Cd", vec!["ab\u{e000}cd"]),
        ("e\u{0301}cole", vec!["e\u{0301}cole"]),
        ("\u{0301}ecole", vec!["ecole"]),
        ("Straße running", vec!["straße", "running"]),
        ("alpha\0beta", vec!["alpha", "beta"]),
    ] {
        assert_eq!(terms(&connection, input).unwrap(), expected, "{input:?}");
    }
    // Mtavruli is unassigned in SQLite's Unicode 6.1 tables. unicode61 includes
    // its fallback category 0, so it stays in the run, without modern case folding.
    // Rust's uppercase predicate still supplies the reviewed part boundary.
    assert_eq!(
        terms(&connection, "ab\u{1c90}cd").unwrap(),
        ["ab\u{1c90}cd", "ab", "\u{1c90}cd"]
    );
}

#[test]
fn filters_dedup_and_budget_apply_once_to_the_entire_input() {
    let connection = Connection::open_in_memory().unwrap();
    assert_eq!(
        terms(
            &connection,
            "fooBar foo BAR theFunction AND OR NOT NEAR a 2"
        )
        .unwrap(),
        ["foobar", "foo", "bar", "thefunction", "not", "near"]
    );
    let mut words = vec!["the".to_owned(), "a".to_owned(), "HTTPServer2".to_owned()];
    for suffix in b'a'..=b'z' {
        words.push(format!("term{}", char::from(suffix)));
    }
    words.extend(["fooBar".to_owned(), "neverRetained".to_owned()]);
    let retained = terms(&connection, &words.join(" ")).unwrap();
    assert_eq!(retained.len(), 32);
    assert_eq!(&retained[..3], ["httpserver2", "http", "server"]);
    assert_eq!(&retained[29..], ["foobar", "foo", "bar"]);
    assert!(!retained.iter().any(|term| term.contains("never")));
}

#[test]
fn cards_and_bound_query_expressions_share_the_same_transform() {
    let connection = Connection::open_in_memory().unwrap();
    let input = "HTTPServer2 fooBar foo BAR";
    assert_eq!(
        card_terms(&connection, input).unwrap(),
        "httpserver2 http server foobar foo bar"
    );
    assert_eq!(
        query_expression(&connection, input).unwrap().as_deref(),
        Some("\"httpserver2\" OR \"http\" OR \"server\" OR \"foobar\" OR \"foo\" OR \"bar\"")
    );
    assert_eq!(card_terms(&connection, "the AND a _").unwrap(), "");
    assert_eq!(query_expression(&connection, "the AND a _").unwrap(), None);
    let expression = query_expression(&connection, "\" OR NEAR(foo*) -bar")
        .unwrap()
        .unwrap();
    assert_eq!(expression, "\"near\" OR \"foo\" OR \"bar\"");
    connection
        .execute_batch("CREATE VIRTUAL TABLE lexical USING fts5(text, tokenize='porter unicode61 remove_diacritics 2')")
        .unwrap();
    connection
        .execute(
            "INSERT INTO lexical(text) VALUES (?1)",
            [card_terms(&connection, "café running").unwrap()],
        )
        .unwrap();
    let query = query_expression(&connection, "cafe runs").unwrap().unwrap();
    let hits: i64 = connection
        .query_row(
            "SELECT count(*) FROM lexical WHERE lexical MATCH ?1",
            [query],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
}
