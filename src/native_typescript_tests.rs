//! TypeScript native extraction: routing by path, the shared JavaScript constructs and
//! lookup-key equivalence with JavaScript. These tests never name the parser crate.
use crate::{
    indexer::{IndexOptions, index_workspace_with_native},
    native_evidence::{self, Artifact},
};
use std::sync::{Arc, atomic::AtomicBool};

fn index(files: &[(&str, &str)]) -> Artifact {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        std::fs::write(root.path().join(path), text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    index_workspace_with_native(
        &IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap()
    .1
}

/// `(kind, name)` of every declaration and the spelling of every call, in source order.
fn measured(artifact: &Artifact) -> (Vec<(String, String)>, Vec<String>) {
    let declarations = artifact
        .declarations
        .iter()
        .map(|d| (d.kind.clone(), d.name.clone().unwrap_or_default()))
        .collect();
    let calls = artifact
        .calls
        .iter()
        .map(|c| c.spelling.clone().unwrap_or_default())
        .collect();
    (declarations, calls)
}

fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items
        .iter()
        .map(|(kind, name)| ((*kind).to_owned(), (*name).to_owned()))
        .collect()
}

#[test]
fn lookup_uses_javascript_rule() {
    for name in [
        "plain",
        "caf\u{e9}",
        "cafe\u{301}",
        "\\u0061bc",
        "\\u{62}cd",
        "\u{10400}label",
        "a\\u0020b",
        "\\u0069f",
        "",
    ] {
        assert_eq!(
            native_evidence::lookup("typescript", name).ok(),
            native_evidence::lookup("javascript", name).ok(),
            "{name:?}"
        );
    }
    assert_eq!(
        native_evidence::lookup("typescript", "\\u0061bc").unwrap(),
        "abc"
    );
    assert_ne!(
        native_evidence::lookup("typescript", "caf\u{e9}").unwrap(),
        native_evidence::lookup("typescript", "cafe\u{301}").unwrap(),
        "exact code points, not normalization"
    );
    assert!(native_evidence::lookup("typescript", "a\\u0020b").is_err());
    assert!(native_evidence::lookup("TypeScript", "abc").is_err());
}

const SHARED: &str = "\
function alpha() { return beta(); }
const gamma = function () { return alpha(); };
const delta = () => new Epsilon();
function* zeta() { yield alpha(); }
const eta = { run() { return obj.go(); } };
";

#[test]
fn shared_javascript_constructs() {
    let javascript = measured(&index(&[("a.js", SHARED)]));
    for path in ["a.ts", "a.tsx"] {
        let artifact = index(&[(path, SHARED)]);
        assert!(
            artifact.coverage.iter().all(|c| c.diagnostic.is_none()),
            "{path} parses without a gap"
        );
        assert!(
            artifact
                .coverage
                .iter()
                .all(|c| c.language == "typescript" && c.document_path == path)
        );
        let (declarations, calls) = measured(&artifact);
        assert_eq!(
            declarations,
            pairs(&[
                ("module", ""),
                ("function", "alpha"),
                ("variable", "gamma"),
                ("variable", "delta"),
                ("anonymousFunction", ""),
                ("function", "zeta"),
                ("variable", "eta"),
                ("method", "run"),
            ]),
            "{path}"
        );
        assert_eq!(calls, ["beta", "alpha", "Epsilon", "alpha", "go"], "{path}");
        assert_eq!(
            (declarations, calls),
            javascript,
            "{path} measures exactly as JavaScript"
        );
    }
}

#[test]
fn the_grammar_follows_the_path() {
    let jsx = "const view = <div onClick={() => run()} />;\n";
    let gap = |path| {
        index(&[(path, jsx)])
            .coverage
            .iter()
            .any(|c| c.diagnostic.is_some())
    };
    assert!(!gap("a.tsx"), "JSX parses under the TSX grammar");
    assert!(
        gap("a.ts"),
        "JSX is a parse error under the TypeScript grammar"
    );
}

#[test]
fn declaration_only_documents() {
    use crate::indexer_ts::declaration_only;
    for path in ["a.d.ts", "dir/a.d.mts", "a.b.d.cts", ".d.ts"] {
        assert!(declaration_only(path), "{path}");
    }
    for path in [
        "a.ts",
        "a.tsx",
        "a.mts",
        "a.cts",
        "d.ts",
        "a.D.ts",
        "a.d.TS",
        "a.d.tsx",
        "a.d.js",
        "a.d.ts.map",
    ] {
        assert!(!declaration_only(path), "{path}");
    }
}
