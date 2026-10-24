//! Syntax-only Rust adapter. Never invokes workspace tools or expands macros.
//! Names and containers are lexical hints, not type, trait, cfg or module resolution.
use crate::model::*;
use anyhow::{Context, Result, ensure};
use std::sync::atomic::Ordering;
use tree_sitter::Node;

fn provenance() -> Provenance {
    Provenance {
        source: "tree-sitter".into(),
        semantic: SemanticState::Unavailable,
    }
}
fn range(n: Node<'_>) -> SourceRange {
    SourceRange {
        start_byte: n.start_byte(),
        end_byte: n.end_byte(),
        start_line: n.start_position().row + 1,
        start_column: n.start_position().column + 1,
        end_line: n.end_position().row + 1,
        end_column: n.end_position().column + 1,
    }
}
#[cfg(test)]
pub(crate) fn extract(g: &mut Graph, file: &SourceFile, cancel: &CancelFlag) -> Result<()> {
    extract_with_limits(g, file, cancel, usize::MAX, usize::MAX)
}
/// External browsing bounds the extraction walk, not tree-sitter parsing time.
/// Kept separate so workspace indexing retains its existing behavior.
pub(crate) fn extract_external(
    g: &mut Graph,
    file: &SourceFile,
    cancel: &CancelFlag,
) -> Result<()> {
    extract_with_limits(g, file, cancel, 50_000, 8_000)
}
fn extract_with_limits(
    g: &mut Graph,
    file: &SourceFile,
    cancel: &CancelFlag,
    max_visits: usize,
    max_records: usize,
) -> Result<()> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut cancelled = |_: &tree_sitter::ParseState| {
        if cancel.load(Ordering::Relaxed) {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &file.text.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut cancelled)),
    );
    ensure!(!cancel.load(Ordering::Relaxed), "indexing cancelled");
    let tree = tree.context("Rust parser failed")?;
    if tree.root_node().has_error() {
        g.stats.parse_error_files += 1;
        g.diagnostics.push(Diagnostic {
            path: Some(file.path.clone()),
            code: "parse-error".into(),
            message: "Tree-sitter recovered from invalid Rust; results may be incomplete".into(),
        });
    }
    g.diagnostics.push(Diagnostic { path: Some(file.path.clone()), code: "rust-lexical-only".into(),
        message: "Rust syntax only: attributes/cfg are not evaluated, modules and names are not resolved, macros are opaque; async bodies describe possible execution when polled".into() });
    let module = format!("module:{}", file.path);
    g.nodes.push(Symbol {
        id: module.clone(),
        name: file.path.clone(),
        display_label: None,
        kind: SymbolKind::Module,
        path: file.path.clone(),
        range: range(tree.root_node()),
        parent: None,
        accessor: false,
        provenance: provenance(),
    });
    Extractor {
        g,
        file,
        cancel,
        visits: 0,
        max_visits,
        max_records,
    }
    .walk(tree.root_node(), &module, &[], 0)
}
struct Extractor<'a> {
    g: &'a mut Graph,
    file: &'a SourceFile,
    cancel: &'a CancelFlag,
    visits: usize,
    max_visits: usize,
    max_records: usize,
}
impl Extractor<'_> {
    fn text(&self, n: Node<'_>) -> &str {
        &self.file.text[n.byte_range()]
    }
    fn id(&self, n: Node<'_>, prefix: &str) -> String {
        format!(
            "{}:{}:{}:{}:{}:{}",
            prefix,
            self.file.path,
            self.file.hash,
            n.start_byte(),
            n.end_byte(),
            n.kind()
        )
    }
    fn walk(&mut self, n: Node<'_>, owner: &str, regions: &[String], depth: usize) -> Result<()> {
        ensure!(!self.cancel.load(Ordering::Relaxed), "indexing cancelled");
        ensure!(depth < 512, "Rust nesting exceeds 512 levels");
        // Each node adds at most one symbol, call, or region. Check before processing
        // even inert nodes so both traversal and record allocation remain bounded.
        if self.visits >= self.max_visits
            || self.g.nodes.len() + self.g.calls.len() + self.g.regions.len() >= self.max_records
        {
            self.g.diagnostics.push(Diagnostic {
                path: Some(self.file.path.clone()),
                code: "rust-extraction-truncated".into(),
                message: "Rust extraction truncated at 50000 AST visits or 8000 records; definitions are partial candidates.".into(),
            });
            anyhow::bail!("Rust extraction budget exceeded");
        }
        self.visits += 1;
        // Token trees and attributes can contain call-shaped text, never runtime calls.
        if matches!(
            n.kind(),
            "macro_invocation"
                | "macro_definition"
                | "token_tree"
                | "attribute_item"
                | "inner_attribute_item"
        ) {
            return Ok(());
        }
        let callable = matches!(n.kind(), "function_item" | "closure_expression");
        let container = matches!(
            n.kind(),
            "impl_item" | "trait_item" | "struct_item" | "enum_item" | "union_item" | "mod_item"
        );
        let mut owner = owner.to_owned();
        let mut regions = regions.to_vec();
        if callable || container {
            let name = n
                .child_by_field_name("name")
                .map(|v| self.text(v).to_owned())
                .unwrap_or_else(|| {
                    if n.kind() == "impl_item" {
                        let ty = n
                            .child_by_field_name("type")
                            .map(|v| self.text(v))
                            .unwrap_or("?");
                        let tr = n
                            .child_by_field_name("trait")
                            .map(|v| format!("{} for ", self.text(v)))
                            .unwrap_or_default();
                        format!("impl {tr}{ty}")
                    } else {
                        format!(
                            "<closure@{}:{}>",
                            n.start_position().row + 1,
                            n.start_position().column + 1
                        )
                    }
                });
            let method = n.kind() == "function_item"
                && n.parent()
                    .and_then(|p| p.parent())
                    .is_some_and(|p| matches!(p.kind(), "impl_item" | "trait_item"));
            let kind = if method {
                SymbolKind::Method
            } else if callable {
                SymbolKind::Function
            } else if n.kind() == "mod_item" {
                SymbolKind::Module
            } else {
                SymbolKind::Class
            };
            let id = self.id(n, "syntax");
            self.g.nodes.push(Symbol {
                id: id.clone(),
                name,
                display_label: None,
                kind,
                path: self.file.path.clone(),
                range: range(n),
                parent: Some(owner),
                accessor: false,
                provenance: provenance(),
            });
            owner = id;
            // A nested callable does not inherit the execution context of its definition.
            regions.clear();
        }
        if matches!(
            n.kind(),
            "if_expression"
                | "match_arm"
                | "loop_expression"
                | "while_expression"
                | "for_expression"
                | "async_block"
                | "unsafe_block"
        ) {
            let id = self.id(n, "region");
            self.g.regions.push(ControlRegion {
                id: id.clone(),
                kind: n.kind().into(),
                label: self.text(n).chars().take(140).collect(),
                parent: regions.last().cloned(),
                owner: owner.clone(),
                path: self.file.path.clone(),
                range: range(n),
            });
            regions.push(id);
        }
        if n.kind() == "call_expression" {
            let callee = n
                .child_by_field_name("function")
                .map(|v| self.text(v).to_owned())
                .unwrap_or_default();
            self.g.calls.push(CallSite {
                id: self.id(n, "call"),
                caller: owner.clone(),
                callee_text: (!callee.is_empty()).then_some(callee),
                path: self.file.path.clone(),
                range: range(n),
                callee_range: None,
                ordinal: 0,
                regions: regions.clone(),
                provenance: provenance(),
            });
        }
        let mut cur = n.walk();
        for child in n.named_children(&mut cur) {
            self.walk(child, &owner, &regions, depth + 1)?;
        }
        Ok(())
    }
}

/// Closed #22 syntax categories measured by the Rust adapter, without cfg expansion.
pub(crate) fn native_kind(n: Node<'_>) -> Option<&'static str> {
    Some(match n.kind() {
        "struct_item" | "enum_item" | "trait_item" | "union_item" => "type",
        "impl_item" => "implementation",
        "mod_item" => "namespace",
        "function_item" | "function_signature_item"
            if n.parent()
                .and_then(|body| body.parent())
                .is_some_and(|container| {
                    matches!(container.kind(), "impl_item" | "trait_item")
                }) =>
        {
            "method"
        }
        "function_item" | "function_signature_item" => "function",
        "closure_expression" => "anonymousFunction",
        "field_declaration" => "field",
        "type_item" => "alias",
        "use_as_clause"
            if n.child_by_field_name("alias")
                .is_some_and(|n| n.kind() == "identifier") =>
        {
            "alias"
        }
        "const_item" | "static_item" => "variable",
        "parameter" => "parameter",
        _ => return None,
    })
}

#[cfg(test)]
mod extraction_budget_tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};
    fn file(text: String) -> SourceFile {
        SourceFile {
            path: "fixture.rs".into(),
            hash: "fixture".into(),
            language: "rust".into(),
            text,
        }
    }
    #[test]
    fn external_records_are_bounded_but_workspace_extraction_is_unchanged() {
        let file = file((0..8100).map(|i| format!("fn f{i}() {{}}\n")).collect());
        let cancel = Arc::new(AtomicBool::new(false));
        let mut limited = Graph::default();
        assert!(extract_external(&mut limited, &file, &cancel).is_err());
        assert_eq!(
            limited.nodes.len() + limited.calls.len() + limited.regions.len(),
            8000
        );
        assert!(
            limited
                .diagnostics
                .iter()
                .any(|d| d.code == "rust-extraction-truncated")
        );
        let mut full = Graph::default();
        extract(&mut full, &file, &cancel).unwrap();
        assert_eq!(full.nodes.len(), 8101);
    }
    #[test]
    fn external_visits_are_bounded_even_when_records_are_sparse() {
        let file = file(format!("fn sparse() {{ {} }}", ";".repeat(60_000)));
        let mut graph = Graph::default();
        assert!(extract_external(&mut graph, &file, &Arc::new(AtomicBool::new(false))).is_err());
        assert_eq!(graph.nodes.len(), 2);
        assert!(
            graph
                .diagnostics
                .iter()
                .any(|d| d.code == "rust-extraction-truncated")
        );
    }
}
