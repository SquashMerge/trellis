//! Resolution identity, with positions ranked within their owning document.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Default)]
struct Encoder {
    positions: BTreeMap<usize, BTreeSet<usize>>,
    ranks: BTreeMap<usize, BTreeMap<usize, usize>>,
    hash: Option<Sha256>,
}

impl Encoder {
    fn word(&mut self, value: usize) {
        if let Some(hash) = &mut self.hash {
            hash.update((value as u64).to_le_bytes());
        }
    }
    fn flag(&mut self, value: bool) {
        if let Some(hash) = &mut self.hash {
            hash.update([u8::from(value)]);
        }
    }
    fn text(&mut self, value: &str) {
        self.word(value.len());
        if let Some(hash) = &mut self.hash {
            hash.update(value.as_bytes());
        }
    }
    fn strings(&mut self, values: &[String]) {
        self.word(values.len());
        for value in values {
            self.text(value);
        }
    }
    fn index(&mut self, value: Option<usize>) {
        self.flag(value.is_some());
        if let Some(value) = value {
            self.word(value);
        }
    }
    fn position(&mut self, document: usize, value: usize) {
        if self.hash.is_some() {
            self.word(self.ranks[&document][&value]);
        } else {
            self.positions.entry(document).or_default().insert(value);
        }
    }
    fn declaration(&mut self, declaration: &DeclarationId) {
        let DeclarationId {
            document,
            start,
            end,
            sid,
        } = declaration;
        self.word(*document);
        self.position(*document, *start);
        self.position(*document, *end);
        self.text(sid);
    }
    fn candidate(&mut self, candidate: &Candidate) {
        let Candidate {
            declaration,
            module,
            follow,
            proof,
        } = candidate;
        self.declaration(declaration);
        self.index(*module);
        self.word(*follow);
        self.indices(proof);
    }
    fn indices(&mut self, values: &BTreeSet<usize>) {
        self.word(values.len());
        for value in values {
            self.word(*value);
        }
    }
    fn name(&mut self, (namespace, name): &Name) {
        self.word(usize::from(*namespace));
        self.text(name);
    }
    fn binding(&mut self, binding: &Binding) {
        let Binding {
            candidate,
            public,
            crate_only,
            restricted,
            uncertain,
            own_crate,
        } = binding;
        self.flag(candidate.is_some());
        if let Some(candidate) = candidate {
            self.candidate(candidate);
        }
        self.flag(*public);
        self.flag(*crate_only);
        self.flag(restricted.is_some());
        if let Some(restricted) = restricted {
            self.strings(restricted);
        }
        self.flag(*uncertain);
        self.flag(*own_crate);
    }
    fn imports(&mut self, document: usize, imports: &Imports) {
        let Imports { all, named, stars } = imports;
        self.word(all.len());
        for import in all {
            let Import {
                start,
                end,
                path,
                alias,
                star,
                public,
                crate_only,
                uncertain,
            } = import;
            self.position(document, *start);
            self.position(document, *end);
            self.strings(path);
            self.flag(alias.is_some());
            if let Some(alias) = alias {
                self.text(alias);
            }
            self.flag(*star);
            self.flag(*public);
            self.flag(*crate_only);
            self.flag(*uncertain);
        }
        self.word(named.len());
        for (name, indices) in named {
            self.text(name);
            self.word(indices.len());
            for index in indices {
                self.word(*index);
            }
        }
        self.word(stars.len());
        for index in stars {
            self.word(*index);
        }
    }
    fn scope(&mut self, scope: &Scope) {
        let Scope {
            document,
            start,
            end,
            parent,
            module,
            directory,
            explicit,
            imports,
            opaque,
            cut,
        } = scope;
        self.word(*document);
        self.position(*document, *start);
        self.position(*document, *end);
        self.index(*parent);
        self.word(*module);
        self.text(directory);
        self.word(explicit.len());
        for (name, bindings) in explicit {
            self.name(name);
            self.word(bindings.len());
            for binding in bindings {
                self.binding(binding);
            }
        }
        self.imports(*document, imports);
        self.flag(*opaque);
        self.flag(*cut);
    }
    fn known_macro(&mut self, known: &KnownMacro) {
        match known {
            KnownMacro::Tokio { test, global } => {
                self.word(0);
                self.flag(*test);
                self.flag(*global);
            }
            KnownMacro::AsyncTrait { bare, global } => {
                self.word(1);
                self.flag(*bare);
                self.flag(*global);
            }
        }
    }
    fn catalog(&mut self, catalog: &Catalog) {
        let Catalog {
            scopes,
            scope_indexes,
            files,
            file_declarations,
            roots,
            ambiguous,
            externs,
            uniform,
            proofs,
            variants,
            shadows,
            statement_macros,
        } = catalog;
        self.text("trellis:rust-catalog:v1");
        self.word(scopes.len());
        for scope in scopes {
            self.scope(scope);
        }
        self.word(scope_indexes.len());
        for (document, index) in scope_indexes {
            let ScopeIndex { ranges, max_ends } = index;
            self.word(*document);
            self.word(ranges.len());
            for (scope, start, end) in ranges {
                self.word(*scope);
                self.position(*document, *start);
                self.position(*document, *end);
            }
            self.word(max_ends.len());
            for end in max_ends {
                self.position(*document, *end);
            }
        }
        self.word(files.len());
        for (document, scope) in files {
            self.word(*document);
            self.word(*scope);
        }
        self.word(file_declarations.len());
        for (document, declaration) in file_declarations {
            self.word(*document);
            self.declaration(declaration);
        }
        self.word(roots.len());
        for (scope, roots) in roots {
            self.word(*scope);
            self.indices(roots);
        }
        self.indices(ambiguous);
        self.word(externs.len());
        for (root, names) in externs {
            self.word(*root);
            self.word(names.len());
            for (name, scope) in names {
                self.text(name);
                self.index(*scope);
            }
        }
        self.indices(uniform);
        self.word(proofs.len());
        for proof in proofs {
            let MacroProof {
                scope,
                macros,
                bindings,
                failed,
            } = proof;
            self.word(*scope);
            self.word(macros.len());
            for known in macros {
                self.known_macro(known);
            }
            self.word(bindings.len());
            for (name, index) in bindings {
                self.name(name);
                self.word(*index);
            }
            self.flag(*failed);
        }
        self.word(variants.len());
        for ((document, start, end), names) in variants {
            self.word(*document);
            self.position(*document, *start);
            self.position(*document, *end);
            self.word(names.len());
            for (name, variant) in names {
                let Variant { certain, braced } = variant;
                self.text(name);
                self.flag(*certain);
                self.flag(*braced);
            }
        }
        let MacroShadows {
            rules,
            macro_use,
            decl_macro,
            unread,
        } = shadows;
        self.word(rules.len());
        for rule in rules {
            self.text(rule);
        }
        self.flag(*macro_use);
        self.flag(*decl_macro);
        self.flag(*unread);
        self.word(statement_macros.len());
        for statement in statement_macros {
            let StatementMacro {
                scope,
                owner,
                source,
                path,
                global,
                settled,
                failed,
            } = statement;
            self.word(*scope);
            self.word(*owner);
            match source {
                MacroSource::StandardLibrary => self.word(0),
                MacroSource::Package(name) => {
                    self.word(1);
                    self.text(name);
                }
            }
            self.strings(path);
            self.flag(*global);
            self.flag(*settled);
            self.flag(*failed);
        }
    }
}

impl Catalog {
    pub(in crate::references) fn digest(&self) -> [u8; 32] {
        let mut encoder = Encoder::default();
        encoder.catalog(self);
        encoder.ranks = std::mem::take(&mut encoder.positions)
            .into_iter()
            .map(|(document, positions)| {
                (
                    document,
                    positions
                        .into_iter()
                        .enumerate()
                        .map(|(rank, position)| (position, rank))
                        .collect(),
                )
            })
            .collect();
        encoder.hash = Some(Sha256::new());
        encoder.catalog(self);
        encoder.hash.expect("digest pass").finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn digests(texts: &[&str]) -> Vec<[u8; 32]> {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        for path in ["src/a.rs", "src/b.rs"] {
            std::fs::write(root.path().join(path), "pub fn f() {}\n").unwrap();
        }
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        texts
            .iter()
            .map(|text| {
                std::fs::write(root.path().join("src/lib.rs"), text).unwrap();
                let (graph, native) = crate::indexer::index_workspace_with_native(
                    &crate::indexer::IndexOptions::new(root.path().into()),
                    &identity.record_id,
                    &Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    |_| {},
                )
                .unwrap();
                let documents = graph
                    .files
                    .into_iter()
                    .map(|file| {
                        let key = native
                            .revision
                            .documents
                            .iter()
                            .find(|document| document.key.path == file.path)
                            .unwrap()
                            .key
                            .clone();
                        DocumentInput {
                            declarations: native
                                .declarations
                                .iter()
                                .filter(|declaration| declaration.document == key)
                                .cloned()
                                .collect(),
                            calls: native
                                .calls
                                .iter()
                                .filter(|call| call.document == key)
                                .cloned()
                                .collect(),
                            key,
                            file,
                            semantic: DocumentSemantic::default(),
                        }
                    })
                    .collect();
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
                super::super::super::resolve_all(&snapshot, &mut || Ok(()))
                    .unwrap()
                    .rust_catalog_digest
            })
            .collect()
    }

    fn different(first: &str, second: &str) {
        let digests = digests(&[first, second]);
        assert_ne!(digests[0], digests[1]);
    }

    #[test]
    fn item_change() {
        different("pub fn f() {}", "pub fn g() {}");
    }
    #[test]
    fn use_change() {
        different("use std::fmt;", "use std::io;");
    }
    #[test]
    fn mod_change() {
        different("pub mod a;", "pub mod b;");
    }
    #[test]
    fn macro_change() {
        different(
            "fn f() { println!(\"x\"); }",
            "fn f() { eprintln!(\"x\"); }",
        );
        different(
            "macro_rules! first { () => {} }",
            "macro_rules! second { () => {} }",
        );
    }
    #[test]
    fn variant_change() {
        different("pub enum E { A, B }", "pub enum E { A, C }");
        different("pub enum E { A }", "pub enum E { A {} }");
    }
    #[test]
    fn literal_comment_and_position_shifts_are_stable() {
        let shifted = format!(
            "pub mod a;\n// {}\nuse a::f;\n// shifted\npub fn run() {{ let n = 1; /* shift */ f(); }}\n",
            "comment ".repeat(200)
        );
        let digests = digests(&[
            "pub mod a;\nuse a::f;\npub fn run() { let n = 1; f(); }\n",
            "pub mod a;\nuse a::f;\npub fn run() { let n = 99999; f(); }\n",
            &shifted,
        ]);
        assert_eq!(digests[0], digests[1]);
        assert_eq!(digests[0], digests[2]);
    }

    fn fixture() -> Catalog {
        let declaration = DeclarationId {
            document: 1,
            start: 10,
            end: 20,
            sid: "decl".into(),
        };
        let candidate = Candidate {
            declaration: declaration.clone(),
            module: Some(0),
            follow: 1,
            proof: BTreeSet::from([0]),
        };
        let mut imports = Imports::default();
        imports.push(Import {
            start: 5,
            end: 9,
            path: vec!["crate".into(), "f".into()],
            alias: Some("g".into()),
            star: false,
            public: true,
            crate_only: false,
            uncertain: false,
        });
        Catalog {
            scopes: vec![Scope {
                document: 0,
                start: 0,
                end: 100,
                parent: Some(0),
                module: 0,
                directory: "src".into(),
                explicit: BTreeMap::from([(
                    (VALUE, "f".into()),
                    vec![Binding {
                        candidate: Some(candidate),
                        public: true,
                        crate_only: false,
                        restricted: Some(vec!["crate".into()]),
                        uncertain: false,
                        own_crate: false,
                    }],
                )]),
                imports,
                opaque: false,
                cut: false,
            }],
            scope_indexes: BTreeMap::from([(
                0,
                ScopeIndex {
                    ranges: vec![(0, 0, 100)],
                    max_ends: vec![100],
                },
            )]),
            files: BTreeMap::from([(0, 0)]),
            file_declarations: BTreeMap::from([(1, declaration)]),
            roots: BTreeMap::from([(0, BTreeSet::from([0]))]),
            ambiguous: BTreeSet::from([0]),
            externs: BTreeMap::from([(0, BTreeMap::from([("dep".into(), Some(0))]))]),
            uniform: BTreeSet::from([0]),
            proofs: vec![MacroProof {
                scope: 0,
                macros: vec![
                    KnownMacro::Tokio {
                        test: false,
                        global: false,
                    },
                    KnownMacro::AsyncTrait {
                        bare: false,
                        global: false,
                    },
                ],
                bindings: vec![((VALUE, "f".into()), 0)],
                failed: false,
            }],
            variants: BTreeMap::from([(
                (1, 10, 20),
                BTreeMap::from([(
                    "A".into(),
                    Variant {
                        certain: true,
                        braced: false,
                    },
                )]),
            )]),
            shadows: MacroShadows {
                rules: BTreeSet::from(["macro".into()]),
                macro_use: false,
                decl_macro: false,
                unread: false,
            },
            statement_macros: vec![StatementMacro {
                scope: 0,
                owner: 0,
                source: MacroSource::StandardLibrary,
                path: vec!["println".into()],
                global: false,
                settled: false,
                failed: false,
            }],
        }
    }

    fn binding(catalog: &mut Catalog) -> &mut Binding {
        &mut catalog.scopes[0]
            .explicit
            .get_mut(&(VALUE, "f".into()))
            .unwrap()[0]
    }
    fn candidate(catalog: &mut Catalog) -> &mut Candidate {
        binding(catalog).candidate.as_mut().unwrap()
    }

    #[test]
    fn every_semantic_member_and_nested_nonposition_is_encoded() {
        let baseline = fixture().digest();
        let mutations: &[fn(&mut Catalog)] = &[
            |c| c.scopes.clear(),
            |c| c.scopes[0].document = 2,
            |c| c.scopes[0].start = c.scopes[0].end,
            |c| c.scopes[0].end = c.scopes[0].start,
            |c| c.scopes[0].parent = None,
            |c| c.scopes[0].module = 1,
            |c| c.scopes[0].directory.push('x'),
            |c| c.scopes[0].opaque = true,
            |c| c.scopes[0].cut = true,
            |c| c.scopes[0].explicit.clear(),
            |c| {
                let value = c.scopes[0].explicit.pop_first().unwrap().1;
                c.scopes[0].explicit.insert((TYPE, "f".into()), value);
            },
            |c| {
                let value = c.scopes[0].explicit.pop_first().unwrap().1;
                c.scopes[0].explicit.insert((VALUE, "g".into()), value);
            },
            |c| binding(c).candidate = None,
            |c| binding(c).public = false,
            |c| binding(c).crate_only = true,
            |c| binding(c).restricted = None,
            |c| binding(c).restricted.as_mut().unwrap().push("inner".into()),
            |c| binding(c).uncertain = true,
            |c| binding(c).own_crate = true,
            |c| candidate(c).module = None,
            |c| candidate(c).follow += 1,
            |c| {
                candidate(c).proof.insert(1);
            },
            |c| candidate(c).declaration.document = 2,
            |c| candidate(c).declaration.start = candidate(c).declaration.end,
            |c| candidate(c).declaration.end = candidate(c).declaration.start,
            |c| candidate(c).declaration.sid.push('x'),
            |c| c.scopes[0].imports.all.clear(),
            |c| c.scopes[0].imports.all[0].start = 9,
            |c| c.scopes[0].imports.all[0].end = 5,
            |c| c.scopes[0].imports.all[0].path.push("x".into()),
            |c| c.scopes[0].imports.all[0].alias = None,
            |c| c.scopes[0].imports.all[0].star = true,
            |c| c.scopes[0].imports.all[0].public = false,
            |c| c.scopes[0].imports.all[0].crate_only = true,
            |c| c.scopes[0].imports.all[0].uncertain = true,
            |c| c.scopes[0].imports.named.clear(),
            |c| c.scopes[0].imports.named.get_mut("g").unwrap().push(1),
            |c| c.scopes[0].imports.stars.push(0),
            |c| c.scope_indexes.clear(),
            |c| c.scope_indexes.get_mut(&0).unwrap().ranges[0].0 = 1,
            |c| c.scope_indexes.get_mut(&0).unwrap().ranges[0].1 = 100,
            |c| c.scope_indexes.get_mut(&0).unwrap().ranges[0].2 = 0,
            |c| c.scope_indexes.get_mut(&0).unwrap().max_ends[0] = 0,
            |c| {
                c.files.insert(1, 0);
            },
            |c| {
                c.files.insert(0, 1);
            },
            |c| c.file_declarations.get_mut(&1).unwrap().sid.push('x'),
            |c| c.file_declarations.get_mut(&1).unwrap().start = 20,
            |c| c.file_declarations.get_mut(&1).unwrap().end = 10,
            |c| c.file_declarations.get_mut(&1).unwrap().document = 2,
            |c| {
                c.file_declarations
                    .insert(2, c.file_declarations[&1].clone());
            },
            |c| {
                c.roots.insert(1, BTreeSet::from([0]));
            },
            |c| {
                c.roots.get_mut(&0).unwrap().insert(1);
            },
            |c| c.ambiguous.clear(),
            |c| c.externs.clear(),
            |c| {
                c.externs.insert(1, BTreeMap::new());
            },
            |c| {
                c.externs.get_mut(&0).unwrap().insert("dep".into(), None);
            },
            |c| {
                c.externs
                    .get_mut(&0)
                    .unwrap()
                    .insert("other".into(), Some(0));
            },
            |c| c.uniform.clear(),
            |c| c.proofs.clear(),
            |c| c.proofs[0].scope = 1,
            |c| {
                c.proofs[0].macros[0] = KnownMacro::Tokio {
                    test: true,
                    global: false,
                }
            },
            |c| {
                c.proofs[0].macros[0] = KnownMacro::Tokio {
                    test: false,
                    global: true,
                }
            },
            |c| {
                c.proofs[0].macros[1] = KnownMacro::AsyncTrait {
                    bare: true,
                    global: false,
                }
            },
            |c| {
                c.proofs[0].macros[1] = KnownMacro::AsyncTrait {
                    bare: false,
                    global: true,
                }
            },
            |c| c.proofs[0].bindings[0].0.0 = TYPE,
            |c| c.proofs[0].bindings[0].0.1.push('x'),
            |c| c.proofs[0].bindings[0].1 += 1,
            |c| c.proofs[0].failed = true,
            |c| c.variants.clear(),
            |c| {
                let value = c.variants.pop_first().unwrap().1;
                c.variants.insert((2, 10, 20), value);
            },
            |c| {
                let value = c.variants.pop_first().unwrap().1;
                c.variants.insert((1, 20, 20), value);
            },
            |c| {
                let value = c.variants.pop_first().unwrap().1;
                c.variants.insert((1, 10, 10), value);
            },
            |c| {
                c.variants
                    .get_mut(&(1, 10, 20))
                    .unwrap()
                    .get_mut("A")
                    .unwrap()
                    .certain = false;
            },
            |c| {
                c.variants
                    .get_mut(&(1, 10, 20))
                    .unwrap()
                    .get_mut("A")
                    .unwrap()
                    .braced = true;
            },
            |c| {
                c.variants.get_mut(&(1, 10, 20)).unwrap().insert(
                    "B".into(),
                    Variant {
                        certain: true,
                        braced: false,
                    },
                );
            },
            |c| c.shadows.rules.clear(),
            |c| c.shadows.macro_use = true,
            |c| c.shadows.decl_macro = true,
            |c| c.shadows.unread = true,
            |c| c.statement_macros.clear(),
            |c| c.statement_macros[0].scope = 1,
            |c| c.statement_macros[0].owner = 1,
            |c| c.statement_macros[0].source = MacroSource::Package("anyhow"),
            |c| c.statement_macros[0].path.push("x".into()),
            |c| c.statement_macros[0].global = true,
            |c| c.statement_macros[0].settled = true,
            |c| c.statement_macros[0].failed = true,
        ];
        for (index, mutate) in mutations.iter().enumerate() {
            let mut changed = fixture();
            mutate(&mut changed);
            assert_ne!(baseline, changed.digest(), "semantic mutation {index}");
        }
    }

    #[test]
    fn execution_and_cached_names_are_not_catalog_state() {
        let mut catalog = fixture();
        let before = catalog.digest();
        let mut execution = RustExecution::new(Work::new(1));
        execution.work.used = 99;
        execution.statement_owner = Some(7);
        execution.memos = false;
        let declaration = catalog.file_declarations[&1].clone();
        let names = Arc::new(Names {
            names: BTreeMap::from([(
                (VALUE, "cached".into()),
                Bucket {
                    candidates: BTreeMap::from([(
                        declaration.clone(),
                        Candidate {
                            declaration,
                            module: Some(0),
                            follow: 2,
                            proof: BTreeSet::from([0]),
                        },
                    )]),
                    unknown: true,
                },
            )]),
            unknown_set: true,
        });
        execution
            .path_memo
            .insert((0, vec!["cached".into()]), names.clone());
        execution.export_memo.insert(
            (0, 2, false, false),
            Arc::new(ExportMemo {
                names,
                consulted: Route::from([(0, "cached".into())]),
            }),
        );
        execution.export_traces.push(ExportTrace {
            consulted: Route::new(),
            cycle: true,
        });
        execution
            .prelude_memo
            .insert((0, "cached".into(), false), true);
        execution.import_memo.insert((0, 5), true);
        assert_eq!(before, catalog.digest());
        catalog.resolver(&mut execution).forget_reads();
        assert!(execution.path_memo.is_empty());
        assert!(execution.export_memo.is_empty());
        assert!(execution.prelude_memo.is_empty());
        assert!(execution.import_memo.is_empty());
        assert_eq!(before, catalog.digest());
    }
}
