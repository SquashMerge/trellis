//! Java catalog identity, with positions ranked within their owning document (#1314).
//!
//! Java site resolution reads the catalog alone and keeps no memo: every member is hashed,
//! destructured so a new field fails to compile until it is. Supertypes are not catalog
//! state; interface extraction reads them again in every build.
use super::*;
use crate::references::catalog_digest::Encoder;

fn bucket(encoder: &mut Encoder, bucket: &Bucket) {
    let Bucket {
        declarations,
        unknown,
    } = bucket;
    encoder.word(declarations.len());
    for declaration in declarations {
        encoder.declaration(declaration);
    }
    encoder.flag(*unknown);
}

fn buckets(encoder: &mut Encoder, buckets: &BTreeMap<String, Bucket>) {
    encoder.word(buckets.len());
    for (name, row) in buckets {
        encoder.text(name);
        bucket(encoder, row);
    }
}

fn package(encoder: &mut Encoder, package: &Package) {
    let Package {
        exports,
        declared,
        partial,
        declarations,
    } = package;
    buckets(encoder, exports);
    encoder.names(declared);
    encoder.flag(*partial);
    encoder.word(declarations.len());
    for (name, count) in declarations {
        encoder.text(name);
        encoder.word(*count);
    }
}

fn unit(encoder: &mut Encoder, document: usize, unit: &Unit) {
    let Unit {
        package,
        imports,
        explicit,
        static_explicit,
        top_level,
        blockers,
        on_demand,
        static_unknown,
        cut,
    } = unit;
    encoder.strings(package);
    encoder.word(imports.len());
    for ((start, end), import) in imports {
        let Import {
            path,
            star,
            static_,
        } = import;
        encoder.position(document, *start);
        encoder.position(document, *end);
        encoder.strings(path);
        encoder.flag(*star);
        encoder.flag(*static_);
    }
    encoder.word(explicit.len());
    for (name, paths) in explicit {
        encoder.text(name);
        encoder.word(paths.len());
        for path in paths {
            encoder.strings(path);
        }
    }
    encoder.names(static_explicit);
    encoder.names(top_level);
    encoder.word(blockers.len());
    for (name, row) in blockers {
        let Blockers { ranges, max_ends } = row;
        encoder.text(name);
        encoder.word(ranges.len());
        for (start, end) in ranges {
            encoder.position(document, *start);
            encoder.position(document, *end);
        }
        encoder.word(max_ends.len());
        for end in max_ends {
            encoder.position(document, *end);
        }
    }
    buckets(encoder, on_demand);
    encoder.flag(*static_unknown);
    encoder.flag(*cut);
}

impl Catalog {
    pub(in crate::references) fn digest(&self) -> [u8; 32] {
        Encoder::digest(|encoder| {
            let Self {
                packages,
                units,
                source_sets,
                unplaced,
            } = self;
            encoder.text("trellis:java-catalog:v1");
            encoder.word(packages.len());
            for ((source_set, path), row) in packages {
                encoder.text(source_set);
                encoder.strings(path);
                package(encoder, row);
            }
            encoder.word(units.len());
            for (document, row) in units {
                encoder.word(*document);
                unit(encoder, *document, row);
            }
            encoder.word(source_sets.len());
            for (document, source_set) in source_sets {
                encoder.word(*document);
                encoder.text(source_set);
            }
            encoder.names(unplaced);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTIL: &str = "package com.acme;\n\nimport java.util.List;\n\npublic class Util {\n  public static int run() {\n    return 1;\n  }\n\n  static class Inner {}\n}\n";
    const HELPER: &str = "package com.acme;\n\npublic interface Helper {}\n";
    const MAIN: &str = "package com.app;\n\nimport com.acme.Util;\nimport com.acme.*;\nimport static com.acme.Util.run;\n\npublic class Main {\n  void go() {\n    com.acme.Util.run();\n  }\n}\n";

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
            }
        }
        /// The digest of the fixture with each `(path, text)` replacing or adding a file, and
        /// a `None` text removing it.
        fn digest(&self, edits: &[(&str, Option<&str>)]) -> [u8; 32] {
            let mut files: Vec<(&str, &str)> = vec![
                ("src/com/acme/Util.java", UTIL),
                ("src/com/acme/Helper.java", HELPER),
                ("src/com/app/Main.java", MAIN),
            ];
            for (path, text) in edits {
                files.retain(|(name, _)| name != path);
                if let Some(text) = text {
                    files.push((path, text));
                }
            }
            let snapshot =
                crate::references::catalog_digest::snapshot_in(self.root.path(), &files, &[], None);
            crate::references::resolve_all(&snapshot, &mut || Ok(()))
                .unwrap()
                .java_catalog_digest
        }
    }

    const UTIL_PATH: &str = "src/com/acme/Util.java";
    const HELPER_PATH: &str = "src/com/acme/Helper.java";
    const MAIN_PATH: &str = "src/com/app/Main.java";

    #[test]
    fn catalog_changing_java_edits_change_the_digest() {
        let fixture = Fixture::new();
        let baseline = fixture.digest(&[]);
        assert_eq!(baseline, fixture.digest(&[]), "deterministic");
        for (label, path, text) in [
            (
                "package",
                HELPER_PATH,
                Some(HELPER.replace("package com.acme;", "package com.other;")),
            ),
            (
                "package removed",
                HELPER_PATH,
                Some(HELPER.replace("package com.acme;\n", "")),
            ),
            (
                "single-type import",
                MAIN_PATH,
                Some(MAIN.replace("import com.acme.Util;", "import com.acme.Helper;")),
            ),
            (
                "added import",
                MAIN_PATH,
                Some(MAIN.replace(
                    "import com.acme.*;",
                    "import com.acme.*;\nimport java.util.Map;",
                )),
            ),
            (
                "on-demand import",
                MAIN_PATH,
                Some(MAIN.replace("import com.acme.*;", "import java.util.*;")),
            ),
            (
                "static import",
                MAIN_PATH,
                Some(MAIN.replace(
                    "import static com.acme.Util.run;",
                    "import static com.acme.Util.walk;",
                )),
            ),
            (
                "static on-demand import",
                MAIN_PATH,
                Some(MAIN.replace(
                    "import static com.acme.Util.run;",
                    "import static com.acme.Util.*;",
                )),
            ),
            (
                "new top-level type",
                HELPER_PATH,
                Some(format!("{HELPER}\nclass Extra {{}}\n")),
            ),
            (
                "renamed top-level type",
                HELPER_PATH,
                Some(HELPER.replace("Helper", "Assistant")),
            ),
            (
                "public modifier",
                HELPER_PATH,
                Some(HELPER.replace("public interface", "interface")),
            ),
            (
                "new nested type",
                UTIL_PATH,
                Some(UTIL.replace(
                    "static class Inner {}",
                    "static class Inner {}\n\n  interface Other {}",
                )),
            ),
            (
                "local type in a body",
                UTIL_PATH,
                Some(UTIL.replace("return 1;", "class Local {}\n    return 1;")),
            ),
            (
                "type parameter",
                UTIL_PATH,
                Some(UTIL.replace("public static int run()", "public static <T> int run()")),
            ),
            (
                "duplicate type",
                "src/com/acme/Twin.java",
                Some(HELPER.replace("interface Helper", "class Helper")),
            ),
            ("parse error", HELPER_PATH, Some(HELPER.replace("{}", "{"))),
            ("removed file", HELPER_PATH, None),
        ] {
            assert_ne!(
                baseline,
                fixture.digest(&[(path, text.as_deref())]),
                "{label} changes the catalog"
            );
        }
    }

    #[test]
    fn body_and_position_only_java_edits_keep_the_digest() {
        let fixture = Fixture::new();
        let baseline = fixture.digest(&[]);
        for (label, path, text) in [
            (
                "same-length literal",
                UTIL_PATH,
                UTIL.replace("return 1;", "return 7;"),
            ),
            (
                "body statements",
                UTIL_PATH,
                UTIL.replace(
                    "return 1;",
                    "int total = 0;\n    for (int i = 0; i < 3; i++) {\n      total += i;\n    }\n    return total;",
                ),
            ),
            (
                "comment shifting a nested type",
                UTIL_PATH,
                UTIL.replace(
                    "\n  static class Inner",
                    "\n  // a comment that shifts every later position\n  static class Inner",
                ),
            ),
            (
                "new method",
                UTIL_PATH,
                UTIL.replace(
                    "\n  static class Inner",
                    "\n  public static int walk() {\n    return 2;\n  }\n\n  static class Inner",
                ),
            ),
            (
                "method body in an importer",
                MAIN_PATH,
                MAIN.replace("com.acme.Util.run();", "int value = com.acme.Util.run();\n    System.out.println(value);"),
            ),
        ] {
            assert_eq!(
                baseline,
                fixture.digest(&[(path, Some(&text))]),
                "{label} keeps the catalog"
            );
        }
    }

    fn declaration() -> DeclarationId {
        DeclarationId {
            document: 1,
            start: 10,
            end: 20,
            sid: "decl".into(),
        }
    }

    fn fixture() -> Catalog {
        let exported = Bucket {
            declarations: BTreeSet::from([declaration()]),
            unknown: false,
        };
        let mut blockers = Blockers {
            ranges: vec![(4, 30)],
            max_ends: Vec::new(),
        };
        blockers
            .finish(&mut Work::new(100), &mut || Ok(()))
            .unwrap();
        Catalog {
            packages: BTreeMap::from([(
                ("set".into(), vec!["com".into(), "acme".into()]),
                Package {
                    exports: BTreeMap::from([("Util".into(), exported)]),
                    declared: BTreeSet::from(["Util".into()]),
                    partial: false,
                    declarations: BTreeMap::from([("Util".into(), 1)]),
                },
            )]),
            units: BTreeMap::from([(
                1,
                Unit {
                    package: vec!["com".into(), "acme".into()],
                    imports: BTreeMap::from([(
                        (2, 6),
                        Import {
                            path: vec!["java".into(), "util".into()],
                            star: true,
                            static_: false,
                        },
                    )]),
                    explicit: BTreeMap::from([(
                        "List".into(),
                        BTreeSet::from([vec!["java".into(), "util".into(), "List".into()]]),
                    )]),
                    static_explicit: BTreeSet::from(["run".into()]),
                    top_level: BTreeSet::from(["Util".into()]),
                    blockers: BTreeMap::from([("T".into(), blockers)]),
                    on_demand: BTreeMap::from([(
                        "Util".into(),
                        Bucket {
                            declarations: BTreeSet::from([declaration()]),
                            unknown: false,
                        },
                    )]),
                    static_unknown: false,
                    cut: false,
                },
            )]),
            source_sets: BTreeMap::from([(1, "set".into())]),
            unplaced: BTreeSet::from(["other".into()]),
        }
    }

    fn package(catalog: &mut Catalog) -> &mut Package {
        catalog.packages.values_mut().next().unwrap()
    }
    fn unit(catalog: &mut Catalog) -> &mut Unit {
        catalog.units.values_mut().next().unwrap()
    }
    fn import(catalog: &mut Catalog) -> &mut Import {
        unit(catalog).imports.values_mut().next().unwrap()
    }
    fn exported(catalog: &mut Catalog) -> &mut Bucket {
        package(catalog).exports.values_mut().next().unwrap()
    }
    fn blockers(catalog: &mut Catalog) -> &mut Blockers {
        unit(catalog).blockers.values_mut().next().unwrap()
    }
    fn with_declaration(catalog: &mut Catalog, edit: fn(&mut DeclarationId)) {
        let mut changed = declaration();
        edit(&mut changed);
        exported(catalog).declarations = BTreeSet::from([changed]);
    }

    #[test]
    fn every_catalog_member_and_nested_nonposition_is_encoded() {
        let baseline = fixture().digest();
        assert_eq!(baseline, fixture().digest());
        let mutations: &[fn(&mut Catalog)] = &[
            |c| c.packages.clear(),
            |c| {
                let value = c.packages.pop_first().unwrap().1;
                c.packages
                    .insert(("other".into(), vec!["com".into(), "acme".into()]), value);
            },
            |c| {
                let value = c.packages.pop_first().unwrap().1;
                c.packages.insert(("set".into(), vec!["com".into()]), value);
            },
            |c| package(c).exports.clear(),
            |c| {
                let value = package(c).exports.pop_first().unwrap().1;
                package(c).exports.insert("Other".into(), value);
            },
            |c| exported(c).unknown = true,
            |c| exported(c).declarations.clear(),
            |c| with_declaration(c, |d| d.document = 2),
            |c| with_declaration(c, |d| d.start = 4),
            |c| with_declaration(c, |d| d.end = 30),
            |c| with_declaration(c, |d| d.sid.push('x')),
            |c| {
                package(c).declared.insert("Hidden".into());
            },
            |c| package(c).partial = true,
            |c| {
                package(c).declarations.insert("Util".into(), 2);
            },
            |c| {
                package(c).declarations.insert("Hidden".into(), 1);
            },
            |c| c.units.clear(),
            |c| {
                let value = c.units.pop_first().unwrap().1;
                c.units.insert(2, value);
            },
            |c| unit(c).package.push("inner".into()),
            |c| unit(c).imports.clear(),
            |c| {
                let value = unit(c).imports.pop_first().unwrap().1;
                unit(c).imports.insert((2, 40), value);
            },
            |c| {
                let value = unit(c).imports.pop_first().unwrap().1;
                // Past the blocker's start: a rank change, not only a shift.
                unit(c).imports.insert((5, 6), value);
            },
            |c| import(c).path.push("List".into()),
            |c| import(c).star = false,
            |c| import(c).static_ = true,
            |c| unit(c).explicit.clear(),
            |c| {
                unit(c).explicit.get_mut("List").unwrap().insert(vec![
                    "java".into(),
                    "awt".into(),
                    "List".into(),
                ]);
            },
            |c| {
                let value = unit(c).explicit.pop_first().unwrap().1;
                unit(c).explicit.insert("Set".into(), value);
            },
            |c| {
                unit(c).static_explicit.insert("walk".into());
            },
            |c| unit(c).top_level.clear(),
            |c| unit(c).blockers.clear(),
            |c| {
                let value = unit(c).blockers.pop_first().unwrap().1;
                unit(c).blockers.insert("U".into(), value);
            },
            |c| blockers(c).ranges[0].0 = 2,
            |c| blockers(c).ranges[0].1 = 20,
            |c| blockers(c).ranges.push((4, 30)),
            |c| blockers(c).max_ends[0] = 6,
            |c| unit(c).on_demand.clear(),
            |c| unit(c).on_demand.values_mut().next().unwrap().unknown = true,
            |c| {
                let value = unit(c).on_demand.pop_first().unwrap().1;
                unit(c).on_demand.insert("Other".into(), value);
            },
            |c| unit(c).static_unknown = true,
            |c| unit(c).cut = true,
            |c| c.source_sets.clear(),
            |c| c.source_sets.get_mut(&1).unwrap().push('x'),
            |c| c.unplaced.clear(),
            |c| {
                c.unplaced.insert("set".into());
            },
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut changed = fixture();
            mutate(&mut changed);
            assert_ne!(baseline, changed.digest(), "catalog mutation {index}");
        }
    }

    #[test]
    fn positions_are_encoded_by_rank_within_their_document() {
        let baseline = fixture().digest();
        let mut shifted = fixture();
        let import = unit(&mut shifted).imports.pop_first().unwrap().1;
        unit(&mut shifted).imports.insert((102, 106), import);
        for range in &mut blockers(&mut shifted).ranges {
            range.0 += 100;
            range.1 += 100;
        }
        for end in &mut blockers(&mut shifted).max_ends {
            *end += 100;
        }
        with_declaration(&mut shifted, |d| {
            d.start += 100;
            d.end += 100;
        });
        let mut moved = declaration();
        moved.start += 100;
        moved.end += 100;
        unit(&mut shifted)
            .on_demand
            .values_mut()
            .next()
            .unwrap()
            .declarations = BTreeSet::from([moved]);
        assert_eq!(baseline, shifted.digest());
    }
}
