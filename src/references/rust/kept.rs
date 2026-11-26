//! The Rust catalog kept between builds (#1315).
//!
//! A catalog is built in two halves. Each file's own phases (indexing its declarations and
//! module scope, and scanning its items and blocks) read that file and, of every other file,
//! only whether an earlier one was found to spell `macro_use` or `decl_macro`. The phases
//! after them (macro-rules names, module edges, parents, roots, lost owners, macro proofs and
//! scope indexes) read across files. [`Prepared`] keeps the first half with what each file
//! added and charged. A later build re-runs a changed file's own phases from the same
//! incoming state and splices the result in; when the file keeps its block-scope count and
//! its outgoing state, no other file's result or scope ID moves, so the catalog and its Work
//! are what a full build computes. Otherwise the caller builds the catalog in full.
use super::*;
use std::ops::Range;

/// What one Rust file's own phases added to a catalog, and charged.
#[derive(Clone)]
struct Part {
    index_steps: usize,
    scan_steps: usize,
    /// Its block scopes, `scopes[blocks]`; its file scope is `files[&document]`.
    blocks: Range<usize>,
    proofs: Range<usize>,
    statement_macros: Range<usize>,
    edges: Range<usize>,
    /// `shadows.macro_use` and `shadows.decl_macro` once this file was scanned.
    macro_use: bool,
    decl_macro: bool,
    has_error: bool,
}

/// A Rust catalog after every file's own phases, before the phases that read across files.
/// Its macro-rules names are not kept: each build reads them again from the texts.
#[derive(Clone)]
pub(in crate::references) struct Prepared {
    catalog: Catalog,
    edges: Vec<ModuleEdge>,
    /// By document index, every Rust document in order.
    parts: BTreeMap<usize, Part>,
}

/// An unchanged file's recorded steps, charged as a full build charges them one by one.
fn charge(
    execution: &mut RustExecution,
    steps: usize,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    check()?;
    ensure!(
        execution.work.charge(steps),
        "bounded_work: reference Rust resolution"
    );
    Ok(())
}

impl Prepared {
    /// Every Rust file's own phases, in the order a full build runs them.
    pub(in crate::references) fn build(
        docs: &[&DocumentInput],
        trees: &[Tree],
        workspace: &super::super::cargo_workspace::Workspace,
        work: Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(Self, RustExecution)> {
        let mut catalog = Catalog::empty();
        let mut execution = RustExecution::new(work);
        let has_error: Vec<bool> = trees
            .iter()
            .map(|tree| tree.root_node().has_error())
            .collect();
        let mut parts = BTreeMap::new();
        let mut edges = Vec::new();
        let mut result = catalog.resolver(&mut execution);
        result.read_macro_rules(docs, &has_error, check)?;
        let mut declarations = BTreeMap::new();
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "rust" {
                continue;
            }
            let before = result.execution.work.used;
            result.index_file(
                document,
                doc,
                &trees[document],
                workspace,
                &mut declarations,
                check,
            )?;
            parts.insert(
                document,
                Part {
                    index_steps: result.execution.work.used - before,
                    scan_steps: 0,
                    blocks: 0..0,
                    proofs: 0..0,
                    statement_macros: 0..0,
                    edges: 0..0,
                    macro_use: false,
                    decl_macro: false,
                    has_error: has_error[document],
                },
            );
        }
        for (&document, &scope) in &result.files.clone() {
            let before = (
                result.execution.work.used,
                result.scopes.len(),
                result.proofs.len(),
                result.statement_macros.len(),
                edges.len(),
            );
            result.scan(
                docs,
                trees[document].root_node(),
                scope,
                &declarations,
                &mut edges,
                0,
                check,
            )?;
            // A parser error can hide an item from any scope of its file, or move it to
            // another. No later file's scan reads this file's scopes.
            if has_error[document] {
                for id in std::iter::once(scope).chain(before.1..result.scopes.len()) {
                    result.scopes[id].opaque = true;
                    result.scopes[id].cut = true;
                }
            }
            let part = parts.get_mut(&document).expect("an indexed Rust file");
            part.scan_steps = result.execution.work.used - before.0;
            part.blocks = before.1..result.scopes.len();
            part.proofs = before.2..result.proofs.len();
            part.statement_macros = before.3..result.statement_macros.len();
            part.edges = before.4..edges.len();
            part.macro_use = result.shadows.macro_use;
            part.decl_macro = result.shadows.decl_macro;
        }
        Ok((
            Self {
                catalog,
                edges,
                parts,
            },
            execution,
        ))
    }

    /// This catalog after a change to the Rust files `changed`, each with its new syntax in
    /// `trees` (no other tree is read): their own phases run again and are spliced in, and
    /// every file's macro-rules names are read again. `None` when a changed file's block-scope
    /// count or outgoing state differs, so that other files' results would move: the caller
    /// then builds in full. `docs` is the same document inventory this catalog was built from.
    pub(in crate::references) fn replace(
        &self,
        docs: &[&DocumentInput],
        trees: &[Tree],
        changed: &BTreeSet<usize>,
        workspace: &super::super::cargo_workspace::Workspace,
        work: Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<(Self, RustExecution)>> {
        ensure!(
            changed.iter().all(|document| self.parts.contains_key(document))
                && self.parts.keys().all(|&document| document < docs.len()
                    && docs[document].key.language == "rust"),
            "store_unavailable: kept Rust catalog inventory"
        );
        let files = &self.catalog.files;
        let mut has_error = vec![false; docs.len()];
        for (&document, part) in &self.parts {
            has_error[document] = if changed.contains(&document) {
                trees[document].root_node().has_error()
            } else {
                part.has_error
            };
        }
        let mut catalog = Catalog {
            scopes: Vec::with_capacity(self.catalog.scopes.len()),
            proofs: Vec::with_capacity(self.catalog.proofs.len()),
            statement_macros: Vec::with_capacity(self.catalog.statement_macros.len()),
            files: files.clone(),
            file_declarations: self.catalog.file_declarations.clone(),
            variants: self.catalog.variants.clone(),
            ..Catalog::empty()
        };
        // Every step charges the supplied budget in the order a full build charges it: the
        // macro-rules names, then each file's indexing, then each file's scan. A changed
        // file's phases run on that same budget, so an over-budget catalog stops where a full
        // build stops, with its typed `bounded_work` (#1315 review).
        let mut execution = RustExecution::new(work);
        catalog
            .resolver(&mut execution)
            .read_macro_rules(docs, &has_error, check)?;
        // Every file scope precedes every block scope: a changed file's starts afresh.
        let file_scopes = files.len();
        catalog
            .scopes
            .extend_from_slice(&self.catalog.scopes[..file_scopes]);
        let mut fresh = BTreeMap::new();
        for (&document, part) in &self.parts {
            if !changed.contains(&document) {
                charge(&mut execution, part.index_steps, check)?;
                continue;
            }
            let mut scratch = Catalog::empty();
            let mut scratch_execution = RustExecution::new(execution.work);
            let mut declarations = BTreeMap::new();
            scratch.resolver(&mut scratch_execution).index_file(
                document,
                docs[document],
                &trees[document],
                workspace,
                &mut declarations,
                check,
            )?;
            let mut scope = scratch.scopes.pop().context("kept Rust file scope")?;
            let id = files[&document];
            scope.module = id;
            catalog.scopes[id] = scope;
            match scratch.file_declarations.remove(&document) {
                Some(declaration) => catalog.file_declarations.insert(document, declaration),
                None => catalog.file_declarations.remove(&document),
            };
            catalog
                .variants
                .retain(|&(variant_document, _, _), _| variant_document != document);
            let index_steps = scratch_execution.work.used - execution.work.used;
            execution.work = scratch_execution.work;
            fresh.insert(document, (index_steps, declarations));
        }
        let mut edges = Vec::with_capacity(self.edges.len());
        let mut parts = BTreeMap::new();
        let (mut macro_use, mut decl_macro) = (false, false);
        let mut next = file_scopes;
        for (&document, part) in &self.parts {
            ensure!(
                part.blocks.start == next,
                "store_unavailable: kept Rust catalog scope order"
            );
            next = part.blocks.end;
            let Some((index_steps, declarations)) = fresh.remove(&document) else {
                charge(&mut execution, part.scan_steps, check)?;
                let proofs = catalog.proofs.len();
                catalog
                    .proofs
                    .extend_from_slice(&self.catalog.proofs[part.proofs.clone()]);
                let statement_macros = catalog.statement_macros.len();
                catalog.statement_macros.extend_from_slice(
                    &self.catalog.statement_macros[part.statement_macros.clone()],
                );
                let first_edge = edges.len();
                edges.extend_from_slice(&self.edges[part.edges.clone()]);
                catalog
                    .scopes
                    .extend_from_slice(&self.catalog.scopes[part.blocks.clone()]);
                (macro_use, decl_macro) = (part.macro_use, part.decl_macro);
                parts.insert(
                    document,
                    Part {
                        proofs: proofs..catalog.proofs.len(),
                        statement_macros: statement_macros..catalog.statement_macros.len(),
                        edges: first_edge..edges.len(),
                        ..part.clone()
                    },
                );
                continue;
            };
            // The file's scan from the state a full build scans it in: every earlier file
            // scanned, its scopes numbered after theirs.
            let mut scratch = Catalog {
                scopes: std::mem::take(&mut catalog.scopes),
                shadows: MacroShadows {
                    macro_use,
                    decl_macro,
                    ..MacroShadows::default()
                },
                ..Catalog::empty()
            };
            let mut scratch_execution = RustExecution::new(execution.work);
            let mut scanned = Vec::new();
            scratch.resolver(&mut scratch_execution).scan(
                docs,
                trees[document].root_node(),
                files[&document],
                &declarations,
                &mut scanned,
                0,
                check,
            )?;
            let scan_steps = scratch_execution.work.used - execution.work.used;
            execution.work = scratch_execution.work;
            let blocks = part.blocks.start..scratch.scopes.len();
            if blocks.len() != part.blocks.len()
                || scratch.shadows.macro_use != part.macro_use
                || scratch.shadows.decl_macro != part.decl_macro
            {
                return Ok(None);
            }
            if has_error[document] {
                for id in std::iter::once(files[&document]).chain(blocks.clone()) {
                    scratch.scopes[id].opaque = true;
                    scratch.scopes[id].cut = true;
                }
            }
            catalog.scopes = scratch.scopes;
            let proofs = catalog.proofs.len();
            catalog.proofs.extend(scratch.proofs);
            let statement_macros = catalog.statement_macros.len();
            catalog.statement_macros.extend(scratch.statement_macros);
            let first_edge = edges.len();
            edges.extend(scanned);
            catalog.variants.extend(scratch.variants);
            (macro_use, decl_macro) = (part.macro_use, part.decl_macro);
            parts.insert(
                document,
                Part {
                    index_steps,
                    scan_steps,
                    blocks,
                    proofs: proofs..catalog.proofs.len(),
                    statement_macros: statement_macros..catalog.statement_macros.len(),
                    edges: first_edge..edges.len(),
                    macro_use,
                    decl_macro,
                    has_error: has_error[document],
                },
            );
        }
        ensure!(
            next == self.catalog.scopes.len() && fresh.is_empty(),
            "store_unavailable: kept Rust catalog scope order"
        );
        catalog.shadows.macro_use = macro_use;
        catalog.shadows.decl_macro = decl_macro;
        Ok(Some((
            Self {
                catalog,
                edges,
                parts,
            },
            execution,
        )))
    }

    /// The finished catalog, computed on a copy so that this one can be kept.
    pub(in crate::references) fn finish(
        &self,
        docs: &[&DocumentInput],
        documents: &BTreeMap<DocumentKey, usize>,
        workspace: &super::super::cargo_workspace::Workspace,
        packages: &external::RustPackages,
        execution: RustExecution,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(Catalog, RustExecution)> {
        self.clone()
            .finish_owned(docs, documents, workspace, packages, execution, check)
    }

    /// The finished catalog, when this one is not kept.
    pub(in crate::references) fn finish_owned(
        self,
        docs: &[&DocumentInput],
        documents: &BTreeMap<DocumentKey, usize>,
        workspace: &super::super::cargo_workspace::Workspace,
        packages: &external::RustPackages,
        mut execution: RustExecution,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(Catalog, RustExecution)> {
        let Self {
            mut catalog,
            mut edges,
            parts,
        } = self;
        let mut has_error = vec![false; docs.len()];
        for (document, part) in parts {
            has_error[document] = part.has_error;
        }
        catalog.resolver(&mut execution).finish(
            docs, &has_error, documents, workspace, packages, &mut edges, check,
        )?;
        Ok((catalog, execution))
    }
}

/// #1315 review: a kept catalog's changed files are indexed and scanned on the supplied
/// budget, so an over-budget catalog stops where a full build stops, with its typed error.
#[cfg(all(test, feature = "test-support"))]
mod budget_tests {
    use super::*;
    use crate::references::{Snapshot, parallel_tests::snapshot_in};

    fn inputs(
        snapshot: &Snapshot,
    ) -> (
        Vec<&DocumentInput>,
        Vec<Tree>,
        super::super::super::cargo_workspace::Workspace,
        usize,
    ) {
        let mut check = || Ok(());
        let docs: Vec<&DocumentInput> = snapshot.documents.iter().collect();
        let trees = docs
            .iter()
            .map(|doc| super::super::super::parse(doc, &mut check).unwrap())
            .collect();
        let mut work = Work::new(usize::MAX);
        let workspace = super::super::super::cargo_workspace::Workspace::new(
            &snapshot.manifests,
            &mut work,
            &mut check,
        )
        .unwrap();
        (docs, trees, workspace, work.used)
    }

    fn sorted(mut snapshot: Snapshot) -> Snapshot {
        snapshot.documents.sort_by(|a, b| a.key.cmp(&b.key));
        snapshot
    }

    #[test]
    fn a_changed_file_is_indexed_and_scanned_within_the_supplied_budget() {
        let blocks: String = (0..400)
            .map(|i| format!("    {{ let _ = {i}; }}\n"))
            .collect();
        let before_text = format!("pub fn f() {{\n{blocks}}}\n");
        let after_text = before_text.replacen("let _ = 0;", "let _ = 9;", 1);
        let root = tempfile::tempdir().unwrap();
        let files = [
            ("src/lib.rs", "pub mod a;\n"),
            ("src/a.rs", before_text.as_str()),
        ];
        let before = sorted(snapshot_in(root.path(), &files));
        let after = sorted(snapshot_in(
            root.path(),
            &[("src/a.rs", after_text.as_str())],
        ));
        let (docs, trees, workspace, _) = inputs(&before);
        let (kept, _) = Prepared::build(
            &docs,
            &trees,
            &workspace,
            Work::new(usize::MAX),
            &mut || Ok(()),
        )
        .unwrap();
        let (docs, trees, workspace, manifests) = inputs(&after);
        let changed = BTreeSet::from([docs
            .iter()
            .position(|doc| doc.key.path == "src/a.rs")
            .unwrap()]);
        let budget = |limit: usize| Work {
            used: manifests,
            limit,
        };
        let (_, fresh) = Prepared::build(
            &docs,
            &trees,
            &workspace,
            budget(usize::MAX),
            &mut || Ok(()),
        )
        .unwrap();
        let (_, spliced) = kept
            .replace(
                &docs,
                &trees,
                &changed,
                &workspace,
                budget(usize::MAX),
                &mut || Ok(()),
            )
            .unwrap()
            .expect("a body edit keeps the catalog");
        let total = fresh.work.used;
        assert_eq!(spliced.work.used, total, "the same Work as a full build");
        // The changed file's scan is most of the catalog's Work.
        assert!(total > 4 * (manifests + 400), "{total}");
        for limit in [total, total - 1, total / 2, total / 4] {
            let full = Prepared::build(&docs, &trees, &workspace, budget(limit), &mut || Ok(()));
            let mut checks = 0;
            let replaced = kept.replace(
                &docs,
                &trees,
                &changed,
                &workspace,
                budget(limit),
                &mut || {
                    checks += 1;
                    Ok(())
                },
            );
            match (full, replaced) {
                (Ok((_, full)), Ok(Some((_, replaced)))) => {
                    assert_eq!(replaced.work.used, full.work.used, "{limit}");
                }
                (Err(full), Err(replaced)) => {
                    assert_eq!(replaced.to_string(), full.to_string(), "{limit}");
                    assert!(
                        replaced.to_string().starts_with("bounded_work"),
                        "{replaced}"
                    );
                    // It stopped at the budget: at most one check per step it could charge,
                    // and one per file whose recorded steps it charged at once.
                    assert!(
                        checks <= limit - manifests + 2 * kept.parts.len() + 2,
                        "{checks} checks under a limit of {limit}"
                    );
                }
                (full, replaced) => panic!(
                    "{limit}: full {:?}, replaced {:?}",
                    full.map(|_| ()),
                    replaced.map(|r| r.is_some())
                ),
            }
        }
    }
}
