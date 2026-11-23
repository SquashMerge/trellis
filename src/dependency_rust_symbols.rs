//! Declaration-only Rust syntax catalog. No workspace graphs, calls, regions, or providers.
//!
//! Qualified names describe source layout and lexical nesting, not resolved Rust exports.
use crate::{dependencies::CatalogSymbol, model::SourceRange};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use tree_sitter::Node;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_VISITS: usize = 50_000;
const MAX_DECLARATIONS: usize = 2_000;
const MAX_SIGNATURE_BYTES: usize = 2_048;
const MAX_CRATE_NAME_BYTES: usize = 128;
const MAX_NAME_BYTES: usize = 512;
const MAX_SCOPE_BYTES: usize = 4_096;
const MAX_OWNER_BYTES: usize = 2_048;

#[derive(Debug, Default)]
pub struct Declarations {
    pub symbols: Vec<CatalogSymbol>,
    pub warnings: Vec<String>,
}

pub fn extract(
    package_id: &str,
    crate_name: &str,
    path: &str,
    text: &str,
    source_ref: &str,
) -> Result<Declarations> {
    ensure!(
        text.len() <= MAX_BYTES,
        "Rust declaration source exceeds 2 MiB"
    );
    // Bound repeated metadata before parsing or allocating per-declaration strings.
    if crate_name.len() > MAX_CRATE_NAME_BYTES {
        return Ok(Declarations {
            symbols: vec![],
            warnings: vec![
                "Rust crate name exceeds 128 bytes; file skipped and catalog is partial.".into(),
            ],
        });
    }
    if [package_id, path, source_ref]
        .iter()
        .any(|value| value.len() > MAX_SCOPE_BYTES)
    {
        return Ok(Declarations {
            symbols: vec![],
            warnings: vec!["Rust source identity/path exceeds 4096 bytes; file skipped and catalog is partial.".into()],
        });
    }
    let mut parser = crate::measured_parse::parser();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let tree = parser
        .parse(text, None)
        .context("Rust declaration parser failed")?;
    let mut walker = Walker {
        package_id,
        path,
        text,
        source_ref,
        output: Declarations::default(),
        visits: 0,
        stopped: false,
    };
    walker.warn("Rust declaration syntax candidates only: source-layout paths are not validated exports; visibility, cfg, imports, traits and dispatch are not resolved.");
    if tree.root_node().has_error() {
        walker.warn("Rust parse errors recovered; declaration catalog is partial.");
    }
    let Some(scope) = source_scope(crate_name, path) else {
        walker
            .warn("Rust qualified scope exceeds 4096 bytes; file skipped and catalog is partial.");
        return Ok(walker.output);
    };
    walker.list(tree.root_node(), &scope, None, None, false, 0);
    Ok(walker.output)
}

fn source_scope(crate_name: &str, path: &str) -> Option<String> {
    // Catalog paths are relative to their pinned authority, which may be Cargo home.
    let relative = path
        .rsplit_once("/src/")
        .map(|(_, tail)| tail)
        .unwrap_or_else(|| path.strip_prefix("src/").unwrap_or(path));
    let mut parts: Vec<&str> = relative.split('/').collect();
    if let Some(file) = parts.pop() {
        let stem = file.strip_suffix(".rs").unwrap_or(file);
        if stem != "mod" && !(parts.is_empty() && matches!(stem, "lib" | "main")) {
            parts.push(stem);
        }
    }
    let mut scope = crate_name.replace('-', "_");
    for part in parts {
        if scope.len() + 2 + part.len() > MAX_SCOPE_BYTES {
            return None;
        }
        scope.push_str("::");
        scope.push_str(part);
    }
    Some(scope)
}

struct Walker<'a> {
    package_id: &'a str,
    path: &'a str,
    text: &'a str,
    source_ref: &'a str,
    output: Declarations,
    visits: usize,
    stopped: bool,
}

impl Walker<'_> {
    fn warn(&mut self, message: &str) {
        if !self.output.warnings.iter().any(|w| w == message) {
            self.output.warnings.push(message.into());
        }
    }

    fn text(&self, node: Node<'_>) -> &str {
        &self.text[node.byte_range()]
    }

    fn visit(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        if self.visits >= MAX_VISITS {
            self.warn("Rust declaration AST visit limit (50000) reached; catalog is partial.");
            self.stopped = true;
            return false;
        }
        self.visits += 1;
        true
    }

    // Only declaration containers are traversed. In particular, this is NOT a
    // general recursive AST walk with a filter applied after visiting bodies.
    fn list(
        &mut self,
        node: Node<'_>,
        scope: &str,
        parent: Option<&str>,
        owner: Option<&str>,
        methods: bool,
        depth: usize,
    ) {
        if depth >= MAX_DEPTH {
            self.warn("Rust declaration depth limit (128) reached; catalog is partial.");
            return;
        }
        if !self.visit() {
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if !self.visit() {
                break;
            }
            match child.kind() {
                "attribute_item" | "inner_attribute_item" => {
                    let attr = self.text(child);
                    let cfg = attr.contains("cfg");
                    let path = attr.contains("path");
                    self.warn("Rust attributes are not evaluated or expanded; active declarations are unknown.");
                    if cfg {
                        self.warn(
                            "Rust cfg/cfg_attr conditions are unknown; candidates may be inactive.",
                        );
                    }
                    if path {
                        self.warn("Rust path attributes are not followed; source-layout module paths are unknown candidates.");
                    }
                }
                "use_declaration" | "extern_crate_declaration" => {
                    self.warn("Rust imports/reexports are not resolved; qualified candidates are not export paths.");
                }
                "macro_invocation" | "macro_definition" => {
                    self.warn("Rust macros are opaque and not expanded; generated declarations are absent.");
                }
                "foreign_mod_item" => {
                    if let Some(body) = child.child_by_field_name("body") {
                        self.list(body, scope, parent, owner, methods, depth + 1);
                    }
                }
                "mod_item"
                | "struct_item"
                | "enum_item"
                | "union_item"
                | "trait_item"
                | "impl_item"
                | "function_item"
                | "function_signature_item"
                | "type_item"
                | "associated_type" => {
                    self.declaration(child, scope, parent, owner, methods, depth);
                }
                // Includes const/static initializers, expression statements, ERROR,
                // token trees, enum variants and all other non-declaration forms.
                _ => {}
            }
            if self.stopped {
                break;
            }
        }
    }

    fn declaration(
        &mut self,
        node: Node<'_>,
        scope: &str,
        parent: Option<&str>,
        owner: Option<&str>,
        methods: bool,
        depth: usize,
    ) {
        if self.output.symbols.len() >= MAX_DECLARATIONS {
            self.warn("Rust declaration limit (2000) reached; catalog is partial.");
            self.stopped = true;
            return;
        }
        let kind = match node.kind() {
            "mod_item" => "module",
            "struct_item" => "struct",
            "enum_item" => "enum",
            "union_item" => "union",
            "trait_item" => "trait",
            "impl_item" => "impl",
            "type_item" | "associated_type" => "type",
            _ if methods => "method",
            _ => "function",
        };
        let impl_owner = if kind == "impl" {
            node.child_by_field_name("type").map(|n| self.text(n))
        } else {
            None
        };
        if impl_owner
            .or(owner)
            .is_some_and(|value| value.len() > MAX_OWNER_BYTES)
        {
            self.warn("Rust owner expression exceeds 2048 bytes; declaration subtree skipped and catalog is partial.");
            return;
        }
        let name = if kind == "impl" {
            let tr = node.child_by_field_name("trait").map(|n| self.text(n));
            let ty = impl_owner.unwrap_or("?");
            if 5 + tr.map_or(0, |value| value.len() + 5) + ty.len() > MAX_NAME_BYTES {
                self.warn("Rust declaration name exceeds 512 bytes; declaration subtree skipped and catalog is partial.");
                return;
            }
            match tr {
                Some(tr) => format!("impl {tr} for {ty}"),
                None => format!("impl {ty}"),
            }
        } else {
            let name = node
                .child_by_field_name("name")
                .map(|n| self.text(n))
                .unwrap_or("?");
            if name.len() > MAX_NAME_BYTES {
                self.warn("Rust declaration name exceeds 512 bytes; declaration subtree skipped and catalog is partial.");
                return;
            }
            name.to_owned()
        };
        if scope.len() + 2 + name.len() > MAX_SCOPE_BYTES {
            self.warn("Rust qualified scope exceeds 4096 bytes; declaration subtree skipped and catalog is partial.");
            return;
        }
        let impl_owner = impl_owner.map(str::to_owned);
        let qualified = format!("{scope}::{name}");
        let mut hash = Sha256::new();
        // Length prefixes prevent delimiter ambiguities; package identity is never
        // inferred from a filename. source_ref includes the pinned content hash.
        for part in [
            self.package_id,
            self.source_ref,
            self.path,
            kind,
            &node.start_byte().to_string(),
            &node.end_byte().to_string(),
        ] {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        let id = format!("dependency-symbol:{}", hex::encode(hash.finalize()));
        let signature = self.signature(node);
        self.output.symbols.push(CatalogSymbol {
            id: id.clone(),
            package_id: self.package_id.into(),
            name: name.clone(),
            qualified_name: qualified.clone(),
            kind: kind.into(),
            parent: parent.map(str::to_owned),
            owner_expression: impl_owner.clone().or_else(|| owner.map(str::to_owned)),
            signature,
            source_ref: self.source_ref.into(),
            path: self.path.into(),
            range: SourceRange {
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_line: node.start_position().row + 1,
                start_column: node.start_position().column + 1,
                end_line: node.end_position().row + 1,
                end_column: node.end_position().column + 1,
            },
        });
        // Never enter function/method, struct/enum/union, type or expression bodies.
        if matches!(kind, "module" | "trait" | "impl")
            && let Some(body) = node.child_by_field_name("body")
        {
            let child_owner = match kind {
                "impl" => impl_owner.as_deref(),
                "trait" => Some(name.as_str()),
                _ => None,
            };
            self.list(
                body,
                &qualified,
                Some(&id),
                child_owner,
                matches!(kind, "trait" | "impl"),
                depth + 1,
            );
        }
    }

    fn signature(&mut self, node: Node<'_>) -> String {
        let end = node
            .child_by_field_name("body")
            .map(|n| n.start_byte())
            .unwrap_or(node.end_byte());
        let header = &self.text[node.start_byte()..end];
        // A brace can also start a const generic/type expression. Conservatively
        // stop there rather than retain any executable block in a signature.
        let header = header.split('{').next().unwrap_or("").trim();
        let mut end = header.len().min(MAX_SIGNATURE_BYTES);
        while !header.is_char_boundary(end) {
            end -= 1;
        }
        let signature = header[..end].trim().to_owned();
        if end < header.len() {
            self.warn("Rust declaration signatures truncated to 2048 UTF-8 bytes.");
        }
        signature
    }
}
