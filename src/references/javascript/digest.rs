//! JavaScript and TypeScript catalog identity, with positions ranked within their owning
//! document (#1314).
//!
//! The catalog is what site resolution reads after the build: each import or re-export
//! source's precomputed resolution, and each importer's package descriptors. Units, hazards,
//! the export memos, workspaces and the root tsconfig are the build's resolver state, which
//! the catalog does not keep; the results they produce are hashed. Manifests and the root
//! tsconfig are the manifest and root-config gates' inputs too.
//!
//! A descriptor's `native_revision_id` and `dependency_hash` are provenance, not structure:
//! each core's fresh catalog carries its own, which a retained source's replay reads and
//! charges, and the manifest gate leaves them out too.
use super::*;
use crate::references::catalog_digest::Encoder;

fn package(encoder: &mut Encoder, package: &external::ExternalPackage) {
    let external::ExternalPackage {
        identity,
        ecosystem,
        evidence,
    } = package;
    encoder.identity(identity);
    encoder.text(ecosystem);
    encoder.word(evidence.len());
    for row in evidence {
        match row {
            external::ExternalEvidence::Manifest {
                source_set_id,
                native_revision_id: _,
                dependency_hash: _,
                path,
                content_hash,
                import_root,
                section,
                key,
                version_constraint,
            } => {
                encoder.word(0);
                encoder.text(source_set_id);
                encoder.text(path);
                encoder.text(content_hash);
                encoder.text(import_root);
                encoder.text(section);
                encoder.text(key);
                encoder.optional_text(version_constraint.as_deref());
            }
            external::ExternalEvidence::StandardLibraryCatalog { catalog, entry } => {
                encoder.word(1);
                encoder.text(catalog);
                encoder.text(entry);
            }
        }
    }
}

/// The digest of a request's catalog, absent when no script source was selected.
pub(in crate::references) fn digest(catalog: Option<&Catalog>) -> [u8; 32] {
    Encoder::digest(|encoder| {
        encoder.text("trellis:javascript-catalog:v1");
        encoder.flag(catalog.is_some());
        let Some(Catalog { results, packages }) = catalog else {
            return;
        };
        encoder.word(results.len());
        for (&key, row) in results {
            encoder.resolution(key, row);
        }
        encoder.word(packages.len());
        for ((document, identity), row) in packages {
            encoder.word(*document);
            encoder.identity(identity);
            package(encoder, row);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTIL: &str = "export function util() {\n  return 1;\n}\n\nexport class Box {}\n";
    const APP: &str = "import { util } from './util';\nimport * as all from './index';\n\nexport function app() {\n  return util() + all.Box.length;\n}\n";
    const INDEX: &str = "export * from './util';\n";
    const PLAIN: &str = "export const plain = () => 1;\n\nexport default plain;\n";
    const MAIN: &str =
        "import plain from './plain.js';\nimport leftPad from 'left-pad';\n\nplain(leftPad);\n";
    const PACKAGE: &str = "{\"dependencies\":{\"left-pad\":\"1.0.0\"}}";

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }
        /// The digest of the fixture with each `(path, text)` replacing or adding a file, a
        /// `None` text removing it, and `manifest` and `config` as the package manifest and
        /// root tsconfig.
        fn digest(
            &self,
            edits: &[(&str, Option<&str>)],
            manifest: Option<&str>,
            config: Option<&str>,
        ) -> [u8; 32] {
            self.snapshot_digest(edits, manifest, config, |_| {})
        }
        fn snapshot_digest(
            &self,
            edits: &[(&str, Option<&str>)],
            manifest: Option<&str>,
            config: Option<&str>,
            adjust: impl FnOnce(&mut Snapshot),
        ) -> [u8; 32] {
            let mut files: Vec<(&str, &str)> = vec![
                ("web/util.ts", UTIL),
                ("web/app.ts", APP),
                ("web/index.ts", INDEX),
                ("web/plain.js", PLAIN),
                ("web/main.js", MAIN),
            ];
            for (path, text) in edits {
                files.retain(|(name, _)| name != path);
                if let Some(text) = text {
                    files.push((path, text));
                }
            }
            let manifests: Vec<_> = manifest
                .map(|text| ("package.json", text))
                .into_iter()
                .collect();
            let mut snapshot = crate::references::catalog_digest::snapshot_in(
                self.root.path(),
                &files,
                &manifests,
                config,
            );
            adjust(&mut snapshot);
            crate::references::resolve_all(&snapshot, &mut || Ok(()))
                .unwrap()
                .javascript_catalog_digest
        }
        fn baseline(&self) -> [u8; 32] {
            self.digest(&[], Some(PACKAGE), None)
        }
    }

    #[test]
    fn catalog_changing_script_edits_change_the_digest() {
        let fixture = Fixture::new();
        let baseline = fixture.baseline();
        assert_eq!(baseline, fixture.baseline(), "deterministic");
        let required = UTIL.replace("return 1;", "return require('./other');");
        for (label, edits) in [
            (
                "import specifier",
                vec![("web/app.ts", Some(APP.replace("'./util'", "'./plain.js'")))],
            ),
            (
                "added import",
                vec![(
                    "web/app.ts",
                    Some(format!("import {{ plain }} from './plain.js';\n{APP}")),
                )],
            ),
            (
                "removed import",
                vec![(
                    "web/main.js",
                    Some(MAIN.replace("import leftPad from 'left-pad';\n", "")),
                )],
            ),
            (
                "named re-export",
                vec![(
                    "web/index.ts",
                    Some(format!("{INDEX}export {{ plain }} from './plain.js';\n")),
                )],
            ),
            (
                "star re-export source",
                vec![("web/index.ts", Some(INDEX.replace("./util", "./plain.js")))],
            ),
            (
                "re-export a star re-export forwards",
                vec![(
                    "web/util.ts",
                    Some(format!("{UTIL}export {{ plain }} from './plain.js';\n")),
                )],
            ),
            (
                "import-require clause",
                vec![(
                    "web/app.ts",
                    Some(format!("import legacy = require('./plain.js');\n{APP}")),
                )],
            ),
            (
                "require in a forwarded module's body",
                vec![("web/util.ts", Some(required.clone()))],
            ),
            (
                "unresolved specifier",
                vec![(
                    "web/main.js",
                    Some(MAIN.replace("'./plain.js'", "'./missing.js'")),
                )],
            ),
            (
                "new module a specifier now reaches",
                vec![
                    (
                        "web/main.js",
                        Some(MAIN.replace("'./plain.js'", "'./fresh.js'")),
                    ),
                    ("web/fresh.js", Some(PLAIN.to_owned())),
                ],
            ),
        ] {
            let edits: Vec<_> = edits
                .iter()
                .map(|(path, text)| (*path, text.as_deref()))
                .collect();
            assert_ne!(
                baseline,
                fixture.digest(&edits, Some(PACKAGE), None),
                "{label} changes the catalog"
            );
        }
        // A package manifest dependency decides an external target.
        assert_ne!(
            baseline,
            fixture.digest(&[], None, None),
            "dependency removed"
        );
        assert_ne!(
            baseline,
            fixture.digest(
                &[],
                Some("{\"dependencies\":{\"left-pad\":\"2.0.0\"}}"),
                None
            ),
            "dependency version"
        );
        // A root tsconfig `paths` mapping decides a bare specifier's local target.
        let aliased = [(
            "web/main.js",
            Some("import plain from '@web/plain';\nplain();\n"),
        )];
        let paths = |target: &str| {
            format!("{{\"compilerOptions\":{{\"paths\":{{\"@web/*\":[\"{target}\"]}}}}}}")
        };
        let mapped = fixture.digest(&aliased, Some(PACKAGE), Some(&paths("web/*.js")));
        assert_ne!(
            mapped,
            fixture.digest(&aliased, Some(PACKAGE), Some(&paths("lib/*.js"))),
            "paths target"
        );
        assert_ne!(
            mapped,
            fixture.digest(&aliased, Some(PACKAGE), None),
            "no paths"
        );
    }

    #[test]
    fn body_position_and_provenance_only_edits_keep_the_digest() {
        let fixture = Fixture::new();
        let baseline = fixture.baseline();
        for (label, path, text) in [
            (
                "same-length literal",
                "web/util.ts",
                UTIL.replace("return 1;", "return 7;"),
            ),
            (
                "body statements",
                "web/util.ts",
                UTIL.replace(
                    "return 1;",
                    "const values = [1, 2, 3];\n  for (const value of values) {\n    console.log(value);\n  }\n  return values.length;",
                ),
            ),
            (
                "comment shifting later sources",
                "web/app.ts",
                APP.replace(
                    "import * as all",
                    "// a comment that shifts every later position\nimport * as all",
                ),
            ),
            (
                "JavaScript arrow body",
                "web/plain.js",
                PLAIN.replace("() => 1", "() => [1, 2, 3].length"),
            ),
            (
                "JavaScript statements and blank lines",
                "web/main.js",
                MAIN.replace("\nplain(leftPad);", "\n\n\nconst value = plain(leftPad);\nconsole.log(value);"),
            ),
            (
                "dynamic import is not cataloged",
                "web/main.js",
                format!("{MAIN}import('./util.js');\n"),
            ),
            // Targets are documents: a star re-export of `util.ts` still reaches only it.
            (
                "export a star re-export forwards from its own module",
                "web/util.ts",
                format!("{UTIL}\nexport function added() {{}}\n"),
            ),
            (
                "removed export a star re-export forwards from its own module",
                "web/util.ts",
                UTIL.replace("\nexport class Box {}\n", ""),
            ),
            (
                "export from a module only named imports reach",
                "web/plain.js",
                format!("{PLAIN}export const second = 2;\n"),
            ),
        ] {
            assert_eq!(
                baseline,
                fixture.digest(&[(path, Some(&text))], Some(PACKAGE), None),
                "{label} keeps the catalog"
            );
        }
        // Manifest provenance is each core's own, as in the manifest gate.
        assert_eq!(
            baseline,
            fixture.snapshot_digest(&[], Some(PACKAGE), None, |snapshot| {
                for manifest in &mut snapshot.manifests {
                    manifest.native_revision_id = "another native revision".into();
                    manifest.dependency_hash = "another dependency hash".into();
                }
            }),
            "provenance"
        );
    }

    fn package_fixture() -> external::ExternalPackage {
        external::ExternalPackage {
            identity: external::ExternalIdentity {
                kind: external::ExternalKind::ExternalPackage,
                source_set_id: "set".into(),
                language: "javascript".into(),
                package: "left-pad".into(),
                standard_library: false,
            },
            ecosystem: "npm",
            evidence: vec![
                external::ExternalEvidence::Manifest {
                    source_set_id: "set".into(),
                    native_revision_id: "native".into(),
                    dependency_hash: "dependencies".into(),
                    path: "package.json".into(),
                    content_hash: "hash".into(),
                    import_root: String::new(),
                    section: "dependencies".into(),
                    key: "left-pad".into(),
                    version_constraint: Some("1.0.0".into()),
                },
                external::ExternalEvidence::StandardLibraryCatalog {
                    catalog: "node",
                    entry: "fs".into(),
                },
            ],
        }
    }

    fn fixture() -> Catalog {
        let package = package_fixture();
        Catalog {
            results: BTreeMap::from([(
                (1, 2, 6),
                SiteResolution {
                    targets: BTreeSet::from([
                        Target::Native(2),
                        Target::External(package.identity.clone()),
                    ]),
                    proof: BTreeSet::from([2]),
                    residual: Some("reason"),
                },
            )]),
            packages: BTreeMap::from([((1, package.identity.clone()), package)]),
        }
    }

    fn package(catalog: &mut Catalog) -> &mut external::ExternalPackage {
        catalog.packages.values_mut().next().unwrap()
    }
    fn manifest(catalog: &mut Catalog) -> &mut external::ExternalEvidence {
        &mut package(catalog).evidence[0]
    }
    fn result(catalog: &mut Catalog) -> &mut SiteResolution {
        catalog.results.values_mut().next().unwrap()
    }
    fn rekey(catalog: &mut Catalog, edit: fn(&mut (usize, external::ExternalIdentity))) {
        let (mut key, value) = catalog.packages.pop_first().unwrap();
        edit(&mut key);
        catalog.packages.insert(key, value);
    }
    fn evidence(catalog: &mut Catalog, edit: fn(&mut String)) {
        let external::ExternalEvidence::Manifest { section, .. } = manifest(catalog) else {
            unreachable!()
        };
        edit(section);
    }

    #[test]
    fn every_catalog_member_and_nested_nonposition_is_encoded() {
        let baseline = digest(Some(&fixture()));
        assert_eq!(baseline, digest(Some(&fixture())));
        assert_ne!(baseline, digest(None));
        let mutations: &[fn(&mut Catalog)] = &[
            |c| c.results.clear(),
            |c| {
                let value = c.results.pop_first().unwrap().1;
                c.results.insert((2, 2, 6), value);
            },
            |c| {
                let value = c.results.pop_first().unwrap().1;
                c.results.insert((1, 6, 6), value);
            },
            |c| {
                let value = c.results.pop_first().unwrap().1;
                c.results.insert((1, 2, 2), value);
            },
            |c| {
                result(c).targets.remove(&Target::Native(2));
            },
            |c| {
                result(c).targets.insert(Target::Native(3));
            },
            |c| {
                let target = result(c).targets.pop_last().unwrap();
                let Target::External(mut identity) = target else {
                    unreachable!()
                };
                identity.package.push('x');
                result(c).targets.insert(Target::External(identity));
            },
            |c| {
                result(c).proof.insert(3);
            },
            |c| result(c).residual = None,
            |c| result(c).residual = Some("other"),
            |c| c.packages.clear(),
            |c| rekey(c, |key| key.0 = 2),
            |c| rekey(c, |key| key.1.source_set_id.push('x')),
            |c| rekey(c, |key| key.1.language.push('x')),
            |c| rekey(c, |key| key.1.package.push('x')),
            |c| rekey(c, |key| key.1.standard_library = true),
            |c| package(c).identity.package.push('x'),
            |c| package(c).ecosystem = "other",
            |c| package(c).evidence.clear(),
            |c| package(c).evidence.reverse(),
            |c| {
                let external::ExternalEvidence::Manifest { source_set_id, .. } = manifest(c) else {
                    unreachable!()
                };
                source_set_id.push('x');
            },
            |c| {
                let external::ExternalEvidence::Manifest { path, .. } = manifest(c) else {
                    unreachable!()
                };
                path.push('x');
            },
            |c| {
                let external::ExternalEvidence::Manifest { content_hash, .. } = manifest(c) else {
                    unreachable!()
                };
                content_hash.push('x');
            },
            |c| {
                let external::ExternalEvidence::Manifest { import_root, .. } = manifest(c) else {
                    unreachable!()
                };
                import_root.push('x');
            },
            |c| evidence(c, |section| section.push('x')),
            |c| {
                let external::ExternalEvidence::Manifest { key, .. } = manifest(c) else {
                    unreachable!()
                };
                key.push('x');
            },
            |c| {
                let external::ExternalEvidence::Manifest {
                    version_constraint, ..
                } = manifest(c)
                else {
                    unreachable!()
                };
                *version_constraint = None;
            },
            |c| {
                package(c).evidence[1] = external::ExternalEvidence::StandardLibraryCatalog {
                    catalog: "other",
                    entry: "fs".into(),
                }
            },
            |c| {
                package(c).evidence[1] = external::ExternalEvidence::StandardLibraryCatalog {
                    catalog: "node",
                    entry: "path".into(),
                }
            },
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut changed = fixture();
            mutate(&mut changed);
            assert_ne!(baseline, digest(Some(&changed)), "catalog mutation {index}");
        }
        // Provenance is not catalog identity.
        let mut provenance = fixture();
        let external::ExternalEvidence::Manifest {
            native_revision_id,
            dependency_hash,
            ..
        } = manifest(&mut provenance)
        else {
            unreachable!()
        };
        native_revision_id.push_str("-later");
        dependency_hash.push_str("-later");
        assert_eq!(baseline, digest(Some(&provenance)));
    }
}
