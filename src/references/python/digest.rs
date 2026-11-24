//! Python catalog identity, with positions ranked within their owning document (#1314).
//!
//! Site resolution after the build reads only the catalog, so two catalogs with the same
//! digest resolve every retained source's sites alike and charge them the same Work. Every
//! member is destructured, so a new field fails to compile until it is hashed.
use super::*;
use crate::references::catalog_digest::Encoder;

fn declaration(encoder: &mut Encoder, declaration: &DeclarationId) {
    let DeclarationId {
        document,
        start,
        end,
        sid,
    } = declaration;
    encoder.word(*document);
    encoder.position(*document, *start);
    encoder.position(*document, *end);
    encoder.text(sid);
}

fn import(encoder: &mut Encoder, document: usize, import: &Import) {
    let Import {
        start,
        end,
        path,
        dots,
        name,
        alias,
        from,
        star,
        module_scope,
        uncertain,
    } = import;
    encoder.position(document, *start);
    encoder.position(document, *end);
    encoder.strings(path);
    encoder.word(*dots);
    encoder.optional_text(name.as_deref());
    encoder.optional_text(alias.as_deref());
    encoder.flag(*from);
    encoder.flag(*star);
    encoder.flag(*module_scope);
    encoder.flag(*uncertain);
}

fn binding(encoder: &mut Encoder, document: usize, binding: &Binding) {
    let Binding { start, kind } = binding;
    encoder.position(document, *start);
    match kind {
        BindingKind::Direct(id) => {
            encoder.word(0);
            declaration(encoder, id);
        }
        BindingKind::Imported(index) => {
            encoder.word(1);
            encoder.word(*index);
        }
        BindingKind::Alias(original) => {
            encoder.word(2);
            encoder.text(original);
        }
        BindingKind::Unknown => encoder.word(3),
    }
}

fn unit(encoder: &mut Encoder, document: usize, unit: &Unit) {
    let Unit {
        imports,
        bindings,
        all,
        dynamic,
    } = unit;
    encoder.word(imports.len());
    for row in imports {
        import(encoder, document, row);
    }
    encoder.word(bindings.len());
    for (name, rows) in bindings {
        encoder.text(name);
        encoder.word(rows.len());
        for row in rows {
            binding(encoder, document, row);
        }
    }
    match all {
        All::Absent => encoder.word(0),
        All::Literal(names) => {
            encoder.word(1);
            encoder.names(names);
        }
        All::Unknown => encoder.word(2),
    }
    encoder.flag(*dynamic);
}

fn target(encoder: &mut Encoder, target: &Target) {
    match target {
        Target::Native(document) => {
            encoder.word(0);
            encoder.word(*document);
        }
        Target::External(identity) => {
            let external::ExternalIdentity {
                kind,
                source_set_id,
                language,
                package,
                standard_library,
            } = identity;
            encoder.word(1);
            match kind {
                external::ExternalKind::ExternalPackage => encoder.word(0),
            }
            encoder.text(source_set_id);
            encoder.text(language);
            encoder.text(package);
            encoder.flag(*standard_library);
        }
    }
}

fn resolution(encoder: &mut Encoder, resolution: &SiteResolution) {
    let SiteResolution {
        targets,
        proof,
        residual,
    } = resolution;
    encoder.word(targets.len());
    for row in targets {
        target(encoder, row);
    }
    encoder.indices(proof);
    encoder.optional_text(*residual);
}

fn catalog(encoder: &mut Encoder, catalog: &Catalog) {
    let Catalog {
        units,
        source_sets,
        modules,
        membership,
        module_proofs,
        child_names,
        roots,
        source_roots,
        file_declarations,
        test_modules,
        results,
    } = catalog;
    encoder.text("trellis:python-catalog:v1");
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
    encoder.word(modules.len());
    for ((source_set, path), rows) in modules {
        encoder.text(source_set);
        encoder.strings(path);
        encoder.word(rows.len());
        for (document, package, root) in rows {
            encoder.word(*document);
            encoder.flag(*package);
            encoder.word(*root);
        }
    }
    encoder.word(membership.len());
    for (document, package) in membership {
        encoder.word(*document);
        encoder.strings(package);
    }
    encoder.word(module_proofs.len());
    for (document, proof) in module_proofs {
        encoder.word(*document);
        encoder.indices(proof);
    }
    encoder.word(child_names.len());
    for (document, names) in child_names {
        encoder.word(*document);
        encoder.names(names);
    }
    encoder.word(roots.len());
    for (source_set, root) in roots {
        encoder.text(source_set);
        encoder.text(root);
    }
    encoder.word(source_roots.len());
    for (source_set, directories) in source_roots {
        encoder.text(source_set);
        encoder.word(directories.len());
        for (directory, root) in directories {
            encoder.text(directory);
            encoder.word(*root);
        }
    }
    encoder.word(file_declarations.len());
    for (document, id) in file_declarations {
        encoder.word(*document);
        declaration(encoder, id);
    }
    encoder.indices(test_modules);
    encoder.word(results.len());
    for ((document, start, end), row) in results {
        encoder.word(*document);
        encoder.position(*document, *start);
        encoder.position(*document, *end);
        resolution(encoder, row);
    }
}

impl Catalog {
    pub(in crate::references) fn digest(&self) -> [u8; 32] {
        Encoder::digest(|encoder| catalog(encoder, self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Python catalog digest of a full reference resolution of exactly `files`, written
    /// under `root`: one root keeps the source set the same across variants.
    fn digest(root: &std::path::Path, files: &[(&str, &str)]) -> [u8; 32] {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                std::fs::remove_dir_all(path).unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }
        let documents = super::super::tests::documents_in(root, files);
        let snapshot = Snapshot {
            pin: IndexPin {
                index_generation: uuid::Uuid::nil(),
                index_revision: 1,
            },
            documents,
            manifests: Vec::new(),
            root_tsconfig: None,
            evidence_gaps: BTreeMap::new(),
            unparsed: BTreeSet::new(),
        };
        crate::references::resolve_all(&snapshot, &mut || Ok(()))
            .unwrap()
            .python_catalog_digest
    }

    const PACKAGE: &str = "from .core import run\n__all__ = ['run']\n";
    const CORE: &str = "import json\n\n\ndef run():\n    return json.dumps(1)\n\n\nclass Runner:\n    def go(self):\n        return 1\n";
    const MAIN: &str = "import json\nfrom pkg.core import run\nfrom pkg import *\n\n\ndef main():\n    return run()\n";

    /// The fixture with `path` replaced by `text`, or removed when `text` is `None`, and
    /// `extra` added.
    fn edited(
        root: &std::path::Path,
        path: &str,
        text: Option<&str>,
        extra: &[(&str, &str)],
    ) -> [u8; 32] {
        let mut files: Vec<(&str, &str)> = vec![
            ("pkg/__init__.py", PACKAGE),
            ("pkg/core.py", CORE),
            ("main.py", MAIN),
        ];
        files.retain(|(name, _)| *name != path);
        if let Some(text) = text {
            files.push((path, text));
        }
        files.extend_from_slice(extra);
        digest(root, &files)
    }

    fn original(root: &std::path::Path) -> [u8; 32] {
        edited(root, "main.py", Some(MAIN), &[])
    }

    #[test]
    fn catalog_changing_python_edits_change_the_digest() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let baseline = original(root);
        assert_eq!(baseline, original(root), "deterministic");
        for (label, path, text, extra) in [
            (
                "import",
                "main.py",
                Some(MAIN.replace("import json", "import os")),
                vec![],
            ),
            (
                "from-import",
                "main.py",
                Some(MAIN.replace("from pkg.core import run", "from pkg.core import Runner")),
                vec![],
            ),
            (
                "relative from-import",
                "pkg/__init__.py",
                Some(PACKAGE.replace("from .core import run", "from .core import Runner")),
                vec![],
            ),
            (
                "import alias",
                "main.py",
                Some(MAIN.replace("import json", "import json as j")),
                vec![],
            ),
            (
                "local import",
                "pkg/core.py",
                Some(CORE.replace("return 1", "import os")),
                vec![],
            ),
            (
                "top-level def",
                "pkg/core.py",
                Some(format!("{CORE}\n\ndef added():\n    pass\n")),
                vec![],
            ),
            (
                "top-level class",
                "pkg/core.py",
                Some(format!("{CORE}\n\nclass Added:\n    pass\n")),
                vec![],
            ),
            (
                "renamed def",
                "pkg/core.py",
                Some(CORE.replace("def run", "def walk")),
                vec![],
            ),
            (
                "module-level assignment",
                "pkg/core.py",
                Some(format!("{CORE}VALUE = 1\n")),
                vec![],
            ),
            (
                "__all__",
                "pkg/__init__.py",
                Some(PACKAGE.replace("['run']", "['run', 'core']")),
                vec![],
            ),
            ("__init__.py removed", "pkg/__init__.py", None, vec![]),
            (
                "__init__.py emptied",
                "pkg/__init__.py",
                Some(String::new()),
                vec![],
            ),
            (
                "new module",
                "main.py",
                Some(MAIN.to_owned()),
                vec![("pkg/extra.py", "def extra():\n    pass\n")],
            ),
            (
                "new package",
                "main.py",
                Some(MAIN.to_owned()),
                vec![("other/__init__.py", "")],
            ),
            (
                "dynamic body",
                "pkg/core.py",
                Some(CORE.replace("return 1", "return eval('1')")),
                vec![],
            ),
            (
                "__all__ written in a body",
                "pkg/core.py",
                Some(CORE.replace("return 1", "__all__.append('x')")),
                vec![],
            ),
        ] {
            assert_ne!(
                baseline,
                edited(root, path, text.as_deref(), &extra),
                "{label} changes the catalog"
            );
        }
    }

    #[test]
    fn body_and_position_only_python_edits_keep_the_digest() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let baseline = original(root);
        for (label, path, text) in [
            (
                "same-length literal",
                "pkg/core.py",
                CORE.replace("dumps(1)", "dumps(7)"),
            ),
            (
                "body statements",
                "pkg/core.py",
                CORE.replace(
                    "return json.dumps(1)",
                    "value = [1, 2, 3]\n    for item in value:\n        print(item)\n    return json.dumps(value)",
                ),
            ),
            (
                "method body",
                "pkg/core.py",
                CORE.replace("return 1", "return self"),
            ),
            (
                "comment",
                "pkg/core.py",
                CORE.replace("\n\n\ndef run", "\n# a comment that shifts every later position\n\ndef run"),
            ),
            (
                "comment and docstring",
                "main.py",
                MAIN.replace(
                    "def main():\n",
                    "def main():\n    \"\"\"Run it.\"\"\"\n    # shifted\n",
                ),
            ),
            (
                "blank lines between imports",
                "main.py",
                MAIN.replace("import json\n", "import json\n\n\n"),
            ),
        ] {
            assert_eq!(
                baseline,
                edited(root, path, Some(&text), &[]),
                "{label} keeps the catalog"
            );
        }
    }

    fn fixture() -> Catalog {
        let declaration = DeclarationId {
            document: 1,
            start: 10,
            end: 20,
            sid: "decl".into(),
        };
        let import = Import {
            start: 2,
            end: 6,
            path: vec!["pkg".into()],
            dots: 1,
            name: Some("run".into()),
            alias: Some("go".into()),
            from: true,
            star: false,
            module_scope: true,
            uncertain: false,
        };
        let unit = Unit {
            imports: vec![import],
            bindings: BTreeMap::from([
                (
                    "go".into(),
                    vec![Binding {
                        start: 2,
                        kind: BindingKind::Imported(0),
                    }],
                ),
                (
                    "run".into(),
                    vec![Binding {
                        start: 10,
                        kind: BindingKind::Direct(declaration.clone()),
                    }],
                ),
            ]),
            all: All::Literal(BTreeSet::from(["run".into()])),
            dynamic: false,
        };
        Catalog {
            units: BTreeMap::from([(1, unit)]),
            source_sets: BTreeMap::from([(1, "set".into())]),
            modules: BTreeMap::from([(("set".into(), vec!["pkg".into()]), vec![(1, true, 0)])]),
            membership: BTreeMap::from([(1, vec!["pkg".into()])]),
            module_proofs: BTreeMap::from([(1, BTreeSet::from([1]))]),
            child_names: BTreeMap::from([(1, BTreeSet::from(["core".into()]))]),
            roots: BTreeSet::from([("set".into(), "pkg".into())]),
            source_roots: BTreeMap::from([("set".into(), BTreeMap::from([("src".into(), 1)]))]),
            file_declarations: BTreeMap::from([(1, declaration)]),
            test_modules: BTreeSet::from([1]),
            results: BTreeMap::from([(
                (1, 2, 6),
                SiteResolution {
                    targets: BTreeSet::from([
                        Target::Native(1),
                        Target::External(external::ExternalIdentity {
                            kind: external::ExternalKind::ExternalPackage,
                            source_set_id: "set".into(),
                            language: "python".into(),
                            package: "json".into(),
                            standard_library: true,
                        }),
                    ]),
                    proof: BTreeSet::from([1]),
                    residual: Some("reason"),
                },
            )]),
        }
    }

    fn unit(catalog: &mut Catalog) -> &mut Unit {
        catalog.units.get_mut(&1).unwrap()
    }
    fn import(catalog: &mut Catalog) -> &mut Import {
        &mut unit(catalog).imports[0]
    }
    fn direct(catalog: &mut Catalog) -> &mut DeclarationId {
        match &mut unit(catalog).bindings.get_mut("run").unwrap()[0].kind {
            BindingKind::Direct(declaration) => declaration,
            _ => unreachable!(),
        }
    }
    fn result(catalog: &mut Catalog) -> &mut SiteResolution {
        catalog.results.get_mut(&(1, 2, 6)).unwrap()
    }
    fn external(catalog: &mut Catalog) -> external::ExternalIdentity {
        let target = result(catalog).targets.pop_last().unwrap();
        let Target::External(identity) = target else {
            unreachable!()
        };
        identity
    }
    fn with_external(catalog: &mut Catalog, edit: fn(&mut external::ExternalIdentity)) {
        let mut identity = external(catalog);
        edit(&mut identity);
        result(catalog).targets.insert(Target::External(identity));
    }

    #[test]
    fn every_catalog_member_and_nested_nonposition_is_encoded() {
        let baseline = fixture().digest();
        assert_eq!(baseline, fixture().digest());
        let mutations: &[fn(&mut Catalog)] = &[
            |c| c.units.clear(),
            |c| {
                let value = c.units.pop_first().unwrap().1;
                c.units.insert(2, value);
            },
            |c| unit(c).imports.clear(),
            // An import that moves past the binding it makes changes their order.
            |c| import(c).start = 11,
            |c| import(c).end = 1,
            |c| import(c).path.push("core".into()),
            |c| import(c).dots = 0,
            |c| import(c).name = None,
            |c| import(c).name = Some("walk".into()),
            |c| import(c).alias = None,
            |c| import(c).from = false,
            |c| import(c).star = true,
            |c| import(c).module_scope = false,
            |c| import(c).uncertain = true,
            |c| unit(c).bindings.clear(),
            |c| {
                let value = unit(c).bindings.remove("go").unwrap();
                unit(c).bindings.insert("gone".into(), value);
            },
            |c| unit(c).bindings.get_mut("go").unwrap()[0].start = 12,
            |c| {
                let row = unit(c).bindings["go"][0].clone();
                unit(c).bindings.get_mut("go").unwrap().push(row);
            },
            |c| unit(c).bindings.get_mut("go").unwrap()[0].kind = BindingKind::Imported(1),
            |c| unit(c).bindings.get_mut("go").unwrap()[0].kind = BindingKind::Unknown,
            |c| unit(c).bindings.get_mut("go").unwrap()[0].kind = BindingKind::Alias("run".into()),
            |c| direct(c).document = 2,
            |c| direct(c).start = 1,
            |c| direct(c).end = 10,
            |c| direct(c).sid.push('x'),
            |c| unit(c).all = All::Absent,
            |c| unit(c).all = All::Unknown,
            |c| unit(c).all = All::Literal(BTreeSet::from(["go".into()])),
            |c| unit(c).dynamic = true,
            |c| c.source_sets.clear(),
            |c| c.source_sets.get_mut(&1).unwrap().push('x'),
            |c| c.modules.clear(),
            |c| {
                let value = c.modules.pop_first().unwrap().1;
                c.modules
                    .insert(("other".into(), vec!["pkg".into()]), value);
            },
            |c| {
                let value = c.modules.pop_first().unwrap().1;
                c.modules.insert(("set".into(), vec!["lib".into()]), value);
            },
            |c| c.modules.values_mut().next().unwrap()[0].0 = 2,
            |c| c.modules.values_mut().next().unwrap()[0].1 = false,
            |c| c.modules.values_mut().next().unwrap()[0].2 = 1,
            |c| c.modules.values_mut().next().unwrap().push((2, false, 0)),
            |c| c.membership.clear(),
            |c| c.membership.get_mut(&1).unwrap().push("core".into()),
            |c| c.module_proofs.clear(),
            |c| {
                c.module_proofs.get_mut(&1).unwrap().insert(2);
            },
            |c| c.child_names.clear(),
            |c| {
                c.child_names.get_mut(&1).unwrap().insert("extra".into());
            },
            |c| c.roots.clear(),
            |c| {
                c.roots.insert(("set".into(), "other".into()));
            },
            |c| c.source_roots.clear(),
            |c| {
                c.source_roots
                    .get_mut("set")
                    .unwrap()
                    .insert("lib".into(), 2);
            },
            |c| {
                c.source_roots
                    .get_mut("set")
                    .unwrap()
                    .insert("src".into(), 2);
            },
            |c| c.file_declarations.clear(),
            |c| c.file_declarations.get_mut(&1).unwrap().sid.push('x'),
            |c| c.file_declarations.get_mut(&1).unwrap().document = 2,
            |c| c.file_declarations.get_mut(&1).unwrap().start = 2,
            |c| c.test_modules.clear(),
            |c| c.results.clear(),
            |c| {
                let value = c.results.pop_first().unwrap().1;
                c.results.insert((1, 10, 20), value);
            },
            |c| {
                let value = c.results.pop_first().unwrap().1;
                c.results.insert((2, 2, 6), value);
            },
            |c| {
                result(c).targets.remove(&Target::Native(1));
            },
            |c| {
                result(c).targets.insert(Target::Native(2));
            },
            |c| {
                external(c);
            },
            |c| with_external(c, |identity| identity.source_set_id.push('x')),
            |c| with_external(c, |identity| identity.language.push('x')),
            |c| with_external(c, |identity| identity.package.push('x')),
            |c| with_external(c, |identity| identity.standard_library = false),
            |c| {
                result(c).proof.insert(2);
            },
            |c| result(c).residual = None,
            |c| result(c).residual = Some("other"),
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
        // Every position of document 1 moves by the same offset: their order is unchanged.
        for row in &mut unit(&mut shifted).imports {
            row.start += 100;
            row.end += 100;
        }
        for rows in unit(&mut shifted).bindings.values_mut() {
            for row in rows {
                row.start += 100;
            }
        }
        direct(&mut shifted).start += 100;
        direct(&mut shifted).end += 100;
        let file = shifted.file_declarations.get_mut(&1).unwrap();
        file.start += 100;
        file.end += 100;
        let value = shifted.results.pop_first().unwrap().1;
        shifted.results.insert((1, 102, 106), value);
        assert_eq!(baseline, shifted.digest());
    }

    #[test]
    fn the_export_memo_is_execution_state_not_catalog_identity() {
        let documents = super::super::tests::documents_with(&[
            ("use.py", "from provider import *\n"),
            ("provider.py", "def public_name():\n    pass\n"),
        ]);
        let docs: Vec<_> = documents.iter().collect();
        let trees: Vec<_> = docs
            .iter()
            .map(|doc| parse(doc, &mut || Ok(())).unwrap())
            .collect();
        let selected: Vec<_> = (0..docs.len()).collect();
        let mut work = Work::new(work::reference_limit(docs.len(), 0));
        let catalog =
            Catalog::new(&docs, &trees, &[], &selected, &mut work, &mut || Ok(())).unwrap();
        let before = catalog.digest();
        let provider = docs
            .iter()
            .position(|doc| doc.key.path == "provider.py")
            .unwrap();
        let resolver = catalog.resolver();
        resolver
            .exports(provider, 1, &mut Route::new(), &mut work, &mut || Ok(()))
            .unwrap();
        assert!(!resolver.export_memo.borrow().is_empty());
        assert_eq!(before, catalog.digest());
        drop(resolver);
        assert!(
            catalog.resolver().export_memo.borrow().is_empty(),
            "each resolver starts cold"
        );
    }
}
