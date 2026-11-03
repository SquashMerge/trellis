//! Discovery of stable lookup behavior over real native publications, without a daemon.
#![cfg(feature = "test-support")]

use proptest::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace_bundle},
    store::Store,
};

#[derive(Clone, Debug)]
struct Case {
    path: &'static str,
    written_type: &'static str,
    key_type: &'static str,
    method: &'static str,
    body: u8,
    operations: Vec<(u8, u8)>,
}

fn cases() -> impl Strategy<Value = Case> {
    (
        prop::sample::select(vec!["src/lib.rs", "src::lib.rs", "src/a::b.rs"]),
        prop::sample::select(vec![
            ("Store", "Store"),
            ("data::Store<u8>", "data::Store"),
            ("crate::data::Store<u8>", "crate::data::Store"),
        ]),
        prop::sample::select(vec!["run", "save"]),
        0u8..10,
        prop::collection::vec((0u8..4, 0u8..10), 0..6),
    )
        .prop_map(
            |(path, (written_type, key_type), method, body, operations)| Case {
                path,
                written_type,
                key_type,
                method,
                body,
                operations,
            },
        )
}

#[derive(Debug)]
struct Expected {
    label: &'static str,
    start: usize,
    end: usize,
    key: String,
}

fn render(case: &Case, body: u8, prefix: bool, duplicate: bool) -> (String, Vec<Expected>) {
    let mut text = format!(
        "struct Store; mod data {{ pub struct Store<T>(T); }}\ntrait First {{ fn {}(); }}\n",
        case.method
    );
    if prefix {
        text.push_str("struct Other; impl Other { fn other() {} }\n");
    }
    let mut expected = Vec::new();
    let key = format!("{}::{}::{}", case.path, case.key_type, case.method);
    for (label, header) in [
        ("original", format!("impl {} {{ ", case.written_type)),
        (
            "duplicate",
            format!("impl First for {} {{ ", case.written_type),
        ),
    ] {
        if label == "duplicate" && !duplicate {
            continue;
        }
        text.push_str(&header);
        let start = text.len();
        text.push_str(&format!("fn {}() {{ let _ = {}; }}", case.method, body));
        let end = text.len();
        expected.push(Expected {
            label,
            start,
            end,
            key: key.clone(),
        });
        text.push_str(" }\n");
    }
    (text, expected)
}

struct World {
    _state: tempfile::TempDir,
    root: tempfile::TempDir,
    store: Store,
    cancel: Arc<AtomicBool>,
}

impl World {
    fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        Self {
            _state: state,
            root,
            store,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn publish_and_check(
        &self,
        case: &Case,
        body: u8,
        prefix: bool,
        duplicate: bool,
    ) -> BTreeMap<&'static str, (String, String)> {
        let (text, expected) = render(case, body, prefix, duplicate);
        let path = self.root.path().join(case.path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &text).unwrap();
        let (graph, native, capture) = index_workspace_bundle(
            &IndexOptions::new(self.root.path().to_owned()),
            self.store.root_id(),
            &self.cancel,
            |_| {},
        )
        .unwrap();
        let leader = self.store.publication_for_tests().unwrap();
        self.store
            .publish_native(
                &graph,
                &capture,
                &native,
                &leader,
                self.store.index_baseline().unwrap(),
                &self.cancel,
            )
            .unwrap();
        drop(leader);

        let read = self.store.evidence_response().unwrap();
        let found = read.mcp_find_with_lines(case.method, 50, None).unwrap();
        assert!(!found.has_more, "case={case:?}");
        let mut matched = BTreeMap::new();
        for model in &expected {
            let hits: Vec<_> = found
                .symbols
                .iter()
                .filter(|hit| {
                    let d = &hit.symbol.declaration;
                    d.document.path == case.path
                        && d.range.start == model.start
                        && d.range.end == model.end
                })
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "rendered declaration missing/duplicated: {model:?}; case={case:?}; source={text}"
            );
            let hit = hits[0];
            assert_eq!(
                hit.source.stable_lookup_key.as_deref(),
                Some(model.key.as_str()),
                "{model:?}; case={case:?}"
            );
            assert_eq!(
                read.mcp_stable_lookup_key(&hit.symbol.declaration.syntax_id, None)
                    .unwrap()
                    .as_deref(),
                Some(model.key.as_str())
            );
            matched.insert(
                model.label,
                (hit.symbol.declaration.syntax_id.clone(), model.key.clone()),
            );
        }
        let keys: BTreeSet<_> = expected.iter().map(|item| item.key.as_str()).collect();
        for key in keys {
            let resolved = read.mcp_resolve_lookup_with_lines(key, None).unwrap();
            assert!(!resolved.has_more);
            let actual: BTreeSet<_> = resolved
                .symbols
                .iter()
                .map(|hit| {
                    let d = &hit.symbol.declaration;
                    assert_eq!(hit.source.stable_lookup_key.as_deref(), Some(key));
                    (d.document.path.clone(), d.range.start, d.range.end)
                })
                .collect();
            let want: BTreeSet<_> = expected
                .iter()
                .filter(|item| item.key == key)
                .map(|item| (case.path.to_owned(), item.start, item.end))
                .collect();
            assert_eq!(
                resolved.symbols.len(),
                want.len(),
                "candidate multiplicity: case={case:?}; source={text}"
            );
            assert_eq!(
                actual, want,
                "candidate labels: case={case:?}; source={text}"
            );
        }
        assert!(
            read.mcp_resolve_lookup_with_lines(&format!("{}::missing", case.path), None)
                .unwrap()
                .symbols
                .is_empty()
        );
        matched
    }
}

fn history(case: &Case) {
    let world = World::new();
    let first = world.publish_and_check(case, case.body, false, false);
    let noop = world.publish_and_check(case, case.body, false, false);
    assert_eq!(first, noop, "no-op: {case:?}");
    let edited = world.publish_and_check(case, (case.body + 1) % 10, false, false);
    assert_eq!(first, edited, "body-only: {case:?}");
    let shifted = world.publish_and_check(case, (case.body + 1) % 10, true, false);
    assert_ne!(
        first["original"].0, shifted["original"].0,
        "ordinal shift: {case:?}"
    );
    assert_eq!(first["original"].1, shifted["original"].1);
    let ambiguous = world.publish_and_check(case, case.body, true, true);
    assert_ne!(ambiguous["original"].0, ambiguous["duplicate"].0);
    assert_eq!(ambiguous["original"].1, ambiguous["duplicate"].1);
    let unique = world.publish_and_check(case, case.body, true, false);
    assert_eq!(unique["original"], ambiguous["original"]);
    let (mut body, mut prefix, mut duplicate) = (case.body, true, false);
    let mut previous = unique;
    for (operation, value) in &case.operations {
        let old_prefix = prefix;
        match operation {
            0 => body = *value,
            1 => prefix = !prefix,
            2 => duplicate = !duplicate,
            _ => {} // Publish the same source again.
        }
        let next = world.publish_and_check(case, body, prefix, duplicate);
        assert_eq!(next["original"].1, previous["original"].1);
        if old_prefix == prefix {
            assert_eq!(next["original"].0, previous["original"].0);
        } else {
            assert_ne!(next["original"].0, previous["original"].0);
        }
        previous = next;
    }
}

#[test]
fn stable_lookup_nonvacuous_history() {
    history(&Case {
        path: "src::lib.rs",
        written_type: "data::Store<u8>",
        key_type: "data::Store",
        method: "run",
        body: 2,
        operations: vec![(0, 3), (2, 0), (1, 0), (2, 0), (3, 0)],
    });
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|n| n.parse().ok()).unwrap_or(24),
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/proptest_stable_lookup.proptest-regressions"))),
        .. ProptestConfig::default()
    })]
    #[test]
    fn stable_lookup_generated_histories(case in cases()) {
        history(&case);
    }
}
