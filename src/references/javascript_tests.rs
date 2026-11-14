use super::*;
use std::sync::{Arc, atomic::AtomicBool};
fn expansion(test: impl FnOnce(&mut Resolver<'_>, usize, &mut Work)) {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in [
        ("use.js", "export * from './provider.js';"),
        (
            "provider.js",
            "export function value() {} export function zebra() {}",
        ),
    ] {
        std::fs::write(root.path().join(path), text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let documents: Vec<_> = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == file.path)
                .unwrap()
                .key
                .clone();
            DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                calls: Vec::new(),
                key,
                file,
            }
        })
        .collect();
    let docs: Vec<_> = documents.iter().collect();
    let trees: Vec<_> = docs
        .iter()
        .map(|doc| parse(doc, &mut || Ok(())).unwrap())
        .collect();
    let document_map: BTreeMap<_, _> = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| (doc.key.clone(), i))
        .collect();
    let provider = docs
        .iter()
        .position(|doc| doc.key.path == "provider.js")
        .unwrap();
    let mut resolver = Resolver {
        docs: &docs,
        trees: &trees,
        documents: &document_map,
        units: BTreeMap::new(),
        hazards: BTreeMap::new(),
        memo: BTreeMap::new(),
        named_memo: BTreeMap::new(),
        workspaces: &js_workspace::Workspaces::default(),
        tsconfig: None,
    };
    let mut work = Work::new(work::reference_limit(docs.len(), 0));
    for (document, doc) in docs.iter().enumerate() {
        let mut native = BTreeMap::<_, Vec<&Declaration>>::new();
        for row in &doc.declarations {
            native
                .entry((row.range.start, row.range.end, row.kind.as_str()))
                .or_default()
                .push(row);
        }
        let unit = inventory(
            doc,
            trees[document].root_node(),
            document,
            &native,
            &mut work,
            &mut || Ok(()),
        )
        .unwrap();
        resolver.units.insert(document, Arc::new(unit));
    }
    test(&mut resolver, provider, &mut work);
}
#[test]
fn immutable_export_memo_hit_and_route_depth_separation() {
    expansion(|resolver, provider, work| {
        let first = resolver
            .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
            .unwrap();
        let hit = resolver
            .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &hit));
        assert_eq!(first.names["value"].candidates.len(), 1);
        let route = BTreeSet::from([(provider, "*".into())]);
        assert!(
            resolver
                .exports(provider, 1, &route, work, &mut || Ok(()))
                .unwrap()
                .unknown_set
        );
        assert!(
            resolver
                .exports(provider, FOLLOW + 1, &Route::new(), work, &mut || Ok(()))
                .unwrap()
                .unknown_set
        );
    });
}
fn first_export_prefix(
    resolver: &mut Resolver<'_>,
    provider: usize,
    work: &mut Work,
) -> (usize, usize) {
    resolver.hazards(provider, work, &mut || Ok(())).unwrap();
    // The same helpers measure one successful native candidate before the next
    // name. Hazards are warm, so neither refusal can stop in the body walk.
    let active = BTreeSet::from([(provider, "*".into())]);
    let first_name = resolver.units[&provider]
        .explicit
        .keys()
        .next()
        .unwrap()
        .clone();
    assert_eq!(resolver.units[&provider].explicit.len(), 2);
    let before = work.used;
    let mut checks = 0;
    let mut check = || {
        checks += 1;
        Ok(())
    };
    charge(Route::new().len() + 1, work, &mut check).unwrap();
    resolver.hazards(provider, work, &mut check).unwrap();
    let first = resolver
        .explicit(provider, &first_name, 1, &active, work, &mut check)
        .unwrap();
    assert_eq!(first.candidates.len(), 1);
    charge_bucket(&first, work, &mut check).unwrap();
    assert!(resolver.memo.is_empty());
    assert!(resolver.named_memo.is_empty());
    (work.used - before, checks)
}
#[test]
fn actual_expansion_uses_accumulated_budget_and_cancellation() {
    expansion(|resolver, provider, work| {
        assert!(work.used > 0);
        let (prefix_work, _) = first_export_prefix(resolver, provider, work);
        assert!(prefix_work > 0);
        work.limit = work.used + prefix_work;
        let before = work.used;
        let error = resolver
            .exports(provider, 1, &Route::new(), work, &mut || Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("bounded_work:"));
        assert!(work.used - before > prefix_work);
        assert!(work.used > work.limit);
        assert!(resolver.memo.is_empty());
        assert!(resolver.named_memo.is_empty());
    });
    expansion(|resolver, provider, work| {
        let (_, prefix_checks) = first_export_prefix(resolver, provider, work);
        let mut checks = 0;
        let before = work.used;
        let error = resolver
            .exports(provider, 1, &Route::new(), work, &mut || {
                checks += 1;
                ensure!(checks <= prefix_checks, "cancelled");
                Ok(())
            })
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "cancelled");
        assert_eq!(checks, prefix_checks + 1);
        assert!(work.used > before);
        assert!(resolver.memo.is_empty());
        assert!(resolver.named_memo.is_empty());
    });
}

fn hazards_for(text: &str) -> (Hazards, usize) {
    let doc = DocumentInput {
        semantic: Default::default(),
        key: DocumentKey {
            source_set_id: "source".into(),
            language: "javascript".into(),
            path: "use.js".into(),
        },
        file: crate::model::SourceFile {
            path: "use.js".into(),
            hash: "hash".into(),
            language: "javascript".into(),
            text: text.into(),
        },
        declarations: Vec::new(),
        calls: Vec::new(),
    };
    let tree = parse(&doc, &mut || Ok(())).unwrap();
    let mut work = Work::new(5_000_000);
    let hazards = body_hazards(&doc, tree.root_node(), &mut work, &mut || Ok(())).unwrap();
    (hazards, work.used)
}

/// Default imports are recorded from their clause, not by asking each
/// identifier for its parent; bindings and charges match named imports.
#[test]
fn default_import_bindings_shadow_commonjs_names_and_charge_like_named_imports() {
    let (unbound, _) = hazards_for("module.exports = 1;");
    assert!(unbound.dynamic);
    let (default, default_used) = hazards_for("import module from './m.js'; module.exports = 1;");
    assert!(!default.dynamic);
    let (named, named_used) = hazards_for("import { module } from './m.js'; module.exports = 1;");
    assert!(!named.dynamic);
    assert_eq!(default_used, named_used);
    let (nested, _) = hazards_for("function f() { const module = 1; } module.exports = 1;");
    assert!(nested.dynamic);
    let (write, _) = hazards_for("import value from './m.js'; value = 2;");
    assert_eq!(write.writes, BTreeSet::from(["value".to_string()]));
}

/// Index `files` and hand the test a resolver over them, with `config` as the root
/// tsconfig of the one source set.
fn script_fixture(
    files: &[(&str, &str)],
    config: Option<&str>,
    test: impl FnOnce(&mut Resolver<'_>, &dyn Fn(&str) -> usize, &mut Work),
) {
    let root = tempfile::tempdir().unwrap();
    for (path, text) in files {
        let path = root.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let identity =
        crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
            .unwrap();
    let (graph, native) = crate::indexer::index_workspace_with_native(
        &crate::indexer::IndexOptions::new(root.path().to_owned()),
        &identity.record_id,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let mut documents: Vec<_> = graph
        .files
        .into_iter()
        .map(|file| {
            let key = native
                .revision
                .documents
                .iter()
                .find(|doc| doc.key.path == file.path)
                .unwrap()
                .key
                .clone();
            DocumentInput {
                semantic: Default::default(),
                declarations: native
                    .declarations
                    .iter()
                    .filter(|row| row.document == key)
                    .cloned()
                    .collect(),
                calls: Vec::new(),
                key,
                file,
            }
        })
        .collect();
    documents.sort_by(|a, b| a.key.cmp(&b.key));
    let docs: Vec<_> = documents.iter().collect();
    let trees: Vec<_> = docs
        .iter()
        .map(|doc| parse(doc, &mut || Ok(())).unwrap())
        .collect();
    let document_map: BTreeMap<_, _> = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| (doc.key.clone(), i))
        .collect();
    let source_set = docs[0].key.source_set_id.clone();
    let mut resolver = Resolver {
        docs: &docs,
        trees: &trees,
        documents: &document_map,
        units: BTreeMap::new(),
        hazards: BTreeMap::new(),
        memo: BTreeMap::new(),
        named_memo: BTreeMap::new(),
        workspaces: &js_workspace::Workspaces::default(),
        tsconfig: config.map(|text| (source_set, tsconfig::parse(text.as_bytes()))),
    };
    let mut work = Work::new(work::reference_limit(docs.len(), 0));
    for (document, doc) in docs.iter().enumerate() {
        let mut native = BTreeMap::<_, Vec<&Declaration>>::new();
        for row in &doc.declarations {
            native
                .entry((row.range.start, row.range.end, row.kind.as_str()))
                .or_default()
                .push(row);
        }
        let unit = inventory(
            doc,
            trees[document].root_node(),
            document,
            &native,
            &mut work,
            &mut || Ok(()),
        )
        .unwrap();
        resolver.units.insert(document, Arc::new(unit));
    }
    let index = |path: &str| {
        docs.iter()
            .position(|doc| doc.key.path == path)
            .unwrap_or_else(|| panic!("{path} is not indexed"))
    };
    test(&mut resolver, &index, &mut work);
}
/// The path a specifier resolves to from `from`, or its reason.
fn resolve_from(
    resolver: &Resolver<'_>,
    index: &dyn Fn(&str) -> usize,
    work: &mut Work,
    from: &str,
    specifier: &str,
) -> std::result::Result<String, &'static str> {
    resolver
        .target(index(from), Some(specifier), work, &mut || Ok(()))
        .unwrap()
        .map(|target| resolver.docs[target].key.path.clone())
}
fn check(files: &[(&str, &str)], config: Option<&str>, cases: &[(&str, &str, Result<&str, &str>)]) {
    script_fixture(files, config, |resolver, index, work| {
        for (from, specifier, expected) in cases {
            assert_eq!(
                resolve_from(resolver, index, work, from, specifier),
                expected.map(str::to_owned),
                "{from}: {specifier}"
            );
        }
    });
}
const EMPTY: &str = "export const a = 1;";

#[test]
fn resolves_extensionless_to_typescript() {
    check(
        &[
            ("a.ts", EMPTY),
            ("x.ts", EMPTY),
            ("y.js", EMPTY),
            ("z/index.tsx", EMPTY),
            ("w.config.ts", EMPTY),
            ("both.ts", EMPTY),
            ("both.tsx", EMPTY),
        ],
        None,
        &[
            ("a.ts", "./x", Ok("x.ts")),
            ("a.ts", "./y", Ok("y.js")),
            ("a.ts", "./z", Ok("z/index.tsx")),
            ("a.ts", "./w.config", Ok("w.config.ts")),
            ("a.ts", "./both", Err("module_path_ambiguous")),
            ("a.ts", "./absent", Err("module_path_not_indexed")),
        ],
    );
}

#[test]
fn js_specifier_maps_to_ts() {
    check(
        &[
            ("a.ts", EMPTY),
            ("b.js", EMPTY),
            ("m.ts", EMPTY),
            ("both.js", EMPTY),
            ("both.ts", EMPTY),
            ("view.tsx", EMPTY),
        ],
        None,
        &[
            ("a.ts", "./m.js", Ok("m.ts")),
            ("a.ts", "./both.js", Ok("both.ts")),
            ("b.js", "./both.js", Ok("both.js")),
            ("b.js", "./m.js", Ok("m.ts")),
            ("a.ts", "./view.js", Ok("view.tsx")),
        ],
    );
}

#[test]
fn module_extension_mapping() {
    check(
        &[
            ("a.ts", EMPTY),
            ("x.mts", EMPTY),
            ("y.cts", EMPTY),
            ("z.tsx", EMPTY),
            ("w.d.mts", "export declare const w: number;"),
            ("v.d.cts", "export declare const v: number;"),
        ],
        None,
        &[
            ("a.ts", "./x.mjs", Ok("x.mts")),
            ("a.ts", "./y.cjs", Ok("y.cts")),
            ("a.ts", "./z.jsx", Ok("z.tsx")),
            ("a.ts", "./w.mjs", Ok("w.d.mts")),
            ("a.ts", "./v.cjs", Ok("v.d.cts")),
            ("a.ts", "./x.cjs", Err("module_path_not_indexed")),
        ],
    );
}

#[test]
fn explicit_ts_extension_literal() {
    check(
        &[
            ("a.ts", EMPTY),
            ("m.ts", EMPTY),
            ("m.js", EMPTY),
            ("n.d.ts", "export declare const n: number;"),
            ("o.mts", EMPTY),
            ("o.cts", EMPTY),
            ("p.tsx", EMPTY),
        ],
        None,
        &[
            ("a.ts", "./m.ts", Ok("m.ts")),
            ("a.ts", "./m.tsx", Err("module_path_not_indexed")),
            ("a.ts", "./n.d.ts", Ok("n.d.ts")),
            ("a.ts", "./o.mts", Ok("o.mts")),
            ("a.ts", "./o.cts", Ok("o.cts")),
            ("a.ts", "./p.tsx", Ok("p.tsx")),
        ],
    );
}

#[test]
fn directory_index() {
    check(
        &[
            ("a.ts", EMPTY),
            ("dir.ts", EMPTY),
            ("dir/index.ts", EMPTY),
            ("only/index.d.ts", "export declare const o: number;"),
            ("legacy/index.js", EMPTY),
            ("nested/a.ts", EMPTY),
            ("index.ts", EMPTY),
        ],
        None,
        &[
            ("a.ts", "./dir/", Ok("dir/index.ts")),
            ("a.ts", "./dir", Err("module_path_ambiguous")),
            ("a.ts", "./only/", Ok("only/index.d.ts")),
            ("a.ts", "./legacy/", Ok("legacy/index.js")),
            ("nested/a.ts", "../", Ok("index.ts")),
            ("a.ts", "./nested/", Err("module_path_not_indexed")),
        ],
    );
}

#[test]
fn declaration_file_is_last_resort() {
    check(
        &[
            ("a.ts", EMPTY),
            ("x.js", EMPTY),
            ("x.d.ts", "export declare const a: number;"),
            ("y.d.ts", "export declare const a: number;"),
        ],
        None,
        &[
            ("a.ts", "./x", Ok("x.js")),
            ("a.ts", "./x.js", Ok("x.js")),
            ("a.ts", "./y", Ok("y.d.ts")),
            ("a.ts", "./y.js", Ok("y.d.ts")),
        ],
    );
    script_fixture(
        &[
            ("a.ts", "export * from './y';"),
            ("y.d.ts", "export declare function f(): void;"),
        ],
        None,
        |resolver, index, work| {
            let names = resolver
                .exports(index("y.d.ts"), 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert_eq!(names.names["f"].candidates.len(), 1);
        },
    );
}

#[test]
fn type_only_imports_resolve() {
    script_fixture(
        &[
            (
                "use.ts",
                "import type { A } from './a';\nimport { type B, c } from './a';\nexport type { A as AA } from './a';\nimport type D from './a';\nexport * from './a';",
            ),
            (
                "a.ts",
                "export interface A {}\nexport interface B {}\nexport const c = 1;\nexport default class D {}",
            ),
        ],
        None,
        |resolver, index, work| {
            let unit = Arc::clone(&resolver.units[&index("use.ts")]);
            assert_eq!(unit.sources.len(), 5);
            for source in &unit.sources {
                let target = resolver
                    .target(
                        index("use.ts"),
                        source.specifier.as_deref(),
                        work,
                        &mut || Ok(()),
                    )
                    .unwrap();
                assert_eq!(target, Ok(index("a.ts")));
            }
            resolver
                .hazards(index("use.ts"), work, &mut || Ok(()))
                .unwrap();
            for name in ["A", "B", "c", "D"] {
                let bucket = resolver
                    .binding(
                        index("use.ts"),
                        name,
                        0,
                        &Route::new(),
                        work,
                        &mut || Ok(()),
                    )
                    .unwrap();
                assert!(!bucket.unknown, "{name}");
                assert_eq!(bucket.candidates.len(), 1, "{name}");
            }
        },
    );
}

const CONFIG: &str = r#"{
      // comment
      "compilerOptions": {
        "paths": {
          "@app/*": ["src/app/*", "lib/*"],
          "@app/special/*": ["special/*"],
          "exact": ["src/exact.ts"],
          "esc/*": ["../out/*"],
          "none/*": ["nothing/*"],
        },
      },
    }"#;

#[test]
fn paths_wildcard_first_match() {
    check(
        &[
            ("a.ts", EMPTY),
            ("src/app/foo.ts", EMPTY),
            ("lib/foo.ts", EMPTY),
            ("lib/bar.ts", EMPTY),
            ("special/x.ts", EMPTY),
            ("src/app/special/x.ts", EMPTY),
            ("src/exact.ts", EMPTY),
            ("pkg/index.ts", EMPTY),
        ],
        Some(CONFIG),
        &[
            ("a.ts", "@app/foo", Ok("src/app/foo.ts")),
            ("a.ts", "@app/bar", Ok("lib/bar.ts")),
            ("a.ts", "@app/special/x", Ok("special/x.ts")),
            ("a.ts", "exact", Ok("src/exact.ts")),
            ("a.ts", "esc/a", Err("absolute_or_bare_import")),
            ("a.ts", "none/a", Err("absolute_or_bare_import")),
            ("a.ts", "unmapped", Err("absolute_or_bare_import")),
            ("a.ts", "@app/missing", Err("absolute_or_bare_import")),
        ],
    );
}

#[test]
fn base_url_bare_specifier() {
    check(
        &[
            ("a.ts", EMPTY),
            ("src/util/x.ts", EMPTY),
            ("src/dir/index.ts", EMPTY),
            ("util/x.ts", EMPTY),
        ],
        Some(r#"{"compilerOptions":{"baseUrl":"./src"}}"#),
        &[
            ("a.ts", "util/x", Ok("src/util/x.ts")),
            ("a.ts", "dir/", Ok("src/dir/index.ts")),
            ("a.ts", "lodash", Err("absolute_or_bare_import")),
            ("a.ts", "/abs", Err("absolute_or_bare_import")),
        ],
    );
    check(
        &[("a.ts", EMPTY), ("util/x.ts", EMPTY)],
        Some(r#"{"compilerOptions":{"baseUrl":".."}}"#),
        &[("a.ts", "util/x", Err("absolute_or_bare_import"))],
    );
    check(
        &[("a.ts", EMPTY), ("util/x.ts", EMPTY)],
        Some(r#"{"compilerOptions":{}}"#),
        &[("a.ts", "util/x", Err("absolute_or_bare_import"))],
    );
}

#[test]
fn root_dirs_virtual_merge() {
    check(
        &[
            ("src/views/a.ts", EMPTY),
            ("src/views/local.ts", EMPTY),
            ("generated/views/b.ts", EMPTY),
            ("other/c.ts", EMPTY),
            ("extra/views/d.ts", EMPTY),
        ],
        Some(r#"{"compilerOptions":{"rootDirs":["src","generated","extra"]}}"#),
        &[
            ("src/views/a.ts", "./b", Ok("generated/views/b.ts")),
            ("src/views/a.ts", "./d", Ok("extra/views/d.ts")),
            ("src/views/a.ts", "./local", Ok("src/views/local.ts")),
            (
                "src/views/a.ts",
                "./missing",
                Err("module_path_not_indexed"),
            ),
            ("generated/views/b.ts", "./a", Ok("src/views/a.ts")),
            ("generated/views/b.ts", "../../other/c", Ok("other/c.ts")),
        ],
    );
    check(
        &[("src/a.ts", EMPTY), ("generated/b.ts", EMPTY)],
        Some(r#"{"compilerOptions":{}}"#),
        &[("src/a.ts", "./b", Err("module_path_not_indexed"))],
    );
}

#[test]
fn cross_boundary_imports() {
    check(
        &[
            ("a.ts", EMPTY),
            ("b.js", EMPTY),
            ("t.ts", EMPTY),
            ("u.js", EMPTY),
            ("c.cjs", EMPTY),
        ],
        None,
        &[
            ("b.js", "./t", Ok("t.ts")),
            ("b.js", "./t.js", Ok("t.ts")),
            ("a.ts", "./u", Ok("u.js")),
            ("a.ts", "./u.js", Ok("u.js")),
            ("a.ts", "./c.cjs", Ok("c.cjs")),
        ],
    );
}

#[test]
fn tsconfig_unreadable_typed() {
    check(
        &[
            ("src/a.ts", EMPTY),
            ("src/b.ts", EMPTY),
            ("generated/c.ts", EMPTY),
        ],
        Some("{ not json"),
        &[
            ("src/a.ts", "lodash", Err("tsconfig_unreadable")),
            ("src/a.ts", "@app/x", Err("tsconfig_unreadable")),
            ("src/a.ts", "./b", Ok("src/b.ts")),
            ("src/a.ts", "./missing", Err("module_path_not_indexed")),
            ("src/a.ts", "../generated/c", Ok("generated/c.ts")),
            ("src/a.ts", "/abs", Err("absolute_or_bare_import")),
        ],
    );
}

#[test]
fn existing_unknown_reasons_keep_their_strings() {
    script_fixture(
        &[("a.js", EMPTY), ("b.ts", EMPTY), ("b.tsx", EMPTY)],
        None,
        |resolver, index, work| {
            let reason = |from: &str, specifier: Option<&str>, work: &mut Work| {
                resolver
                    .target(index(from), specifier, work, &mut || Ok(()))
                    .unwrap()
            };
            assert_eq!(reason("a.js", None, work), Err("import_escape_unsupported"));
            assert_eq!(
                reason("a.js", Some("lodash"), work),
                Err("absolute_or_bare_import")
            );
            assert_eq!(
                reason("a.js", Some("../../x"), work),
                Err("relative_import_outside_source")
            );
            assert_eq!(
                reason("a.js", Some("./b"), work),
                Err("module_path_ambiguous")
            );
            assert_eq!(
                reason("a.js", Some("./nope"), work),
                Err("module_path_not_indexed")
            );
        },
    );
}

#[test]
fn import_require_clause_static() {
    script_fixture(
        &[
            (
                "use.ts",
                "import m = require(\"./model\");\nexport const v = m;\nconst w = 1;",
            ),
            ("model.ts", EMPTY),
        ],
        None,
        |resolver, index, work| {
            let use_ts = index("use.ts");
            let unit = Arc::clone(&resolver.units[&use_ts]);
            assert_eq!(unit.sources.len(), 1);
            assert_eq!(unit.sources[0].specifier.as_deref(), Some("./model"));
            assert!(unit.sources[0].supported && !unit.sources[0].star);
            assert!(matches!(
                unit.bindings["m"][0].kind,
                BindingKind::Imported { name: None, .. }
            ));
            assert_eq!(
                resolver
                    .target(use_ts, Some("./model"), work, &mut || Ok(()))
                    .unwrap(),
                Ok(index("model.ts"))
            );
            let sites = extract(
                resolver.docs[use_ts],
                &resolver.trees[use_ts],
                work,
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(sites.len(), 1);
            assert_eq!(sites[0].spelling, "\"./model\"");
            assert_eq!(sites[0].start, unit.sources[0].start);
            // The namespace binding shadows CommonJS names.
            let hazards = body_hazards(
                resolver.docs[use_ts],
                resolver.trees[use_ts].root_node(),
                work,
                &mut || Ok(()),
            )
            .unwrap();
            assert!(!hazards.dynamic);
        },
    );
}

#[test]
fn export_assignment_unknown() {
    script_fixture(
        &[("a.ts", "export = 1;"), ("b.ts", "export const b = 1;")],
        None,
        |resolver, index, _| {
            assert!(resolver.units[&index("a.ts")].unknown_set);
            assert!(!resolver.units[&index("b.ts")].unknown_set);
        },
    );
}

#[test]
fn cts_ends_export_following() {
    script_fixture(
        &[
            (
                "use.ts",
                "export * from './c.cts';\nexport * from './d.mts';",
            ),
            ("c.cts", "export const a = 1;"),
            ("d.mts", "export const b = 1;"),
        ],
        None,
        |resolver, index, work| {
            let c = resolver
                .exports(index("c.cts"), 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert!(c.unknown_set && c.names.is_empty());
            let c = resolver
                .named(index("c.cts"), "a", 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert!(c.unknown);
            let d = resolver
                .exports(index("d.mts"), 1, &Route::new(), work, &mut || Ok(()))
                .unwrap();
            assert!(d.names.contains_key("b") && !d.unknown_set);
        },
    );
}

#[test]
fn typescript_declarations_export() {
    script_fixture(
        &[(
            "decl.ts",
            "export interface I {}\nexport enum E { A }\nexport abstract class C {}\nexport type T = number;\nexport namespace N { export const x = 1; }\nexport function f(a: string): void;\nexport function f(a: number): void;\nexport function f(a: any): void {}\nexport declare const d: number;\nexport declare function g(): void;\ndeclare module \"m\" {}\n",
        )],
        None,
        |resolver, index, work| {
            let decl = index("decl.ts");
            assert!(!resolver.units[&decl].unknown_set);
            for (name, count) in [
                ("I", 1),
                ("E", 1),
                ("C", 1),
                ("T", 1),
                ("N", 1),
                ("f", 1),
                ("d", 1),
                ("g", 1),
            ] {
                let bucket = resolver
                    .named(decl, name, 0, &Route::new(), work, &mut || Ok(()))
                    .unwrap();
                assert!(!bucket.unknown, "{name}");
                assert_eq!(bucket.candidates.len(), count, "{name}");
            }
        },
    );
}

#[test]
fn typescript_dotted_namespace_unknown_set() {
    script_fixture(
        &[("dotted.ts", "export namespace P.Q {}\n")],
        None,
        |resolver, index, _| {
            let dotted = index("dotted.ts");
            assert!(resolver.units[&dotted].unknown_set, "dotted namespace");
        },
    );
}
