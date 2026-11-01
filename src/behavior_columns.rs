//! Which column a sequence participant is drawn in (#558): one per type or module, so a
//! type's methods share a lifeline and the method is named on the arrow. A display grouping
//! over measured declarations: it never changes a step's exact target.
use crate::behavior::{MAX_COLUMNS, Participant, SequenceColumn};
use crate::model::{Symbol, SymbolKind};
use anyhow::Result;
use std::collections::BTreeMap;

/// The declarations of one document, by ID, and its text. Reads nothing: the caller supplies
/// both from the selected snapshot.
pub(crate) struct DocumentColumns {
    pub language: String,
    pub nodes: BTreeMap<String, Symbol>,
    pub text: String,
}

/// A Rust type, whose `impl` blocks may span files, or a class or module of one document.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    RustType { name: String, scope: String },
    Column(SequenceColumn),
}

impl DocumentColumns {
    /// The nearest named type enclosing `symbol`, itself included, or else its module.
    /// A closure or nested function belongs to whatever encloses it. Rust: `impl` type,
    /// trait, struct or enum, else the module; `impl Trait for Foo` belongs to `Foo`.
    /// JavaScript, Java and Python: the class, else the file.
    pub(crate) fn owner(&self, symbol: &Symbol) -> Owner {
        for node in self.ancestry(symbol) {
            if node.kind != SymbolKind::Class {
                continue;
            }
            let named = !node.name.starts_with('<');
            if self.language != "rust" {
                if named {
                    return Owner::Column(SequenceColumn {
                        id: node.id.clone(),
                        label: node.name.clone(),
                    });
                }
                continue;
            }
            let block = self.text.get(node.range.start_byte..node.range.end_byte);
            let name = block.and_then(crate::files_view::impl_block_target);
            if let Some(name) = name.or(named.then(|| node.name.clone())) {
                let scope = self.inline_modules(node, &symbol.path);
                return Owner::RustType {
                    name,
                    scope: format!("{}{scope}", symbol.path),
                };
            }
        }
        let label = file_module(&symbol.path) + &self.inline_modules(symbol, &symbol.path);
        Owner::Column(SequenceColumn {
            id: format!("module:{}:{label}", symbol.path),
            label,
        })
    }

    /// `symbol` and then the declarations enclosing it, innermost first. A parent chain is a
    /// tree; the bound only stops a corrupt row from looping.
    fn ancestry<'a>(&'a self, symbol: &'a Symbol) -> impl Iterator<Item = &'a Symbol> {
        std::iter::successors(Some(symbol), |node| self.nodes.get(node.parent.as_ref()?))
            .take(self.nodes.len() + 1)
    }

    /// The inline modules enclosing `node`, as a `::a::b` suffix of its file's module.
    fn inline_modules(&self, node: &Symbol, path: &str) -> String {
        let modules: Vec<_> = self
            .ancestry(node)
            .filter(|n| n.kind == SymbolKind::Module && n.name != path)
            .map(|n| format!("::{}", n.name))
            .collect();
        modules.into_iter().rev().collect()
    }
}

impl Owner {
    /// The column of this owner. A Rust type's name joins its `impl` blocks across files
    /// when it names one type in the snapshot, the ordinary case. `declarations` counts the
    /// types of that name; when several exist, nothing here says which an `impl` implements,
    /// so its blocks stay apart by file and module rather than conflate two types.
    pub(crate) fn column(
        self,
        declarations: &mut dyn FnMut(&str) -> Result<usize>,
    ) -> Result<SequenceColumn> {
        Ok(match self {
            Owner::Column(column) => column,
            Owner::RustType { name, scope } => SequenceColumn {
                id: if declarations(&name)? > 1 {
                    format!("type:{scope}:{name}")
                } else {
                    format!("type:{name}")
                },
                label: name,
            },
        })
    }
}

/// The module a file is, by the last meaningful segment of its path: `src/a/b.rs` is `b`,
/// `src/a/mod.rs` and `src/a/__init__.py` are `a`. JavaScript keeps its file name.
fn file_module(path: &str) -> String {
    let mut segments = path.rsplit('/');
    let file = segments.next().unwrap_or(path);
    let (stem, extension) = file.rsplit_once('.').unwrap_or((file, ""));
    match (stem, extension) {
        ("mod", "rs") | ("__init__", "py") => segments.next().unwrap_or(stem).to_owned(),
        (_, "rs" | "py") => stem.to_owned(),
        _ => file.to_owned(),
    }
}

/// Whether a target in `column` can be drawn: its column is drawn already, or fits.
pub(crate) fn fits(participants: &[Participant], column: &SequenceColumn) -> bool {
    let mut drawn: Vec<&str> = participants
        .iter()
        .filter_map(|p| p.column.as_ref())
        .map(|c| c.id.as_str())
        .collect();
    drawn.sort_unstable();
    drawn.dedup();
    drawn.contains(&column.id.as_str()) || drawn.len() < MAX_COLUMNS
}

/// Tells apart two columns that would read the same (two `index.js`, or a type name that
/// stayed apart), by the path of each column's first participant.
pub(crate) fn disambiguate(participants: &mut [Participant]) {
    let mut paths: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for participant in participants.iter() {
        if let (Some(column), Some(symbol)) = (&participant.column, &participant.declaration) {
            paths
                .entry(column.label.clone())
                .or_default()
                .entry(column.id.clone())
                .or_insert_with(|| symbol.path.clone());
        }
    }
    for participant in participants {
        if let Some(column) = &mut participant.column
            && let Some(ids) = paths.get(&column.label).filter(|ids| ids.len() > 1)
        {
            column.label = format!("{} ({})", column.label, ids[&column.id]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provenance, SemanticState, SourceRange};

    fn symbol(
        id: &str,
        name: &str,
        kind: SymbolKind,
        parent: Option<&str>,
        start: usize,
    ) -> Symbol {
        Symbol {
            id: id.into(),
            name: name.into(),
            display_label: None,
            kind,
            path: "src/store/graph_read.rs".into(),
            range: SourceRange {
                start_byte: start,
                end_byte: start + 1,
                ..SourceRange::default()
            },
            parent: parent.map(str::to_owned),
            accessor: false,
            provenance: Provenance {
                source: "test".into(),
                semantic: SemanticState::Unavailable,
            },
        }
    }

    fn nodes(symbols: Vec<Symbol>) -> BTreeMap<String, Symbol> {
        symbols.into_iter().map(|s| (s.id.clone(), s)).collect()
    }

    #[test]
    fn file_modules_name_the_module_not_its_entry_file() {
        assert_eq!(file_module("src/store/graph_read.rs"), "graph_read");
        assert_eq!(file_module("src/store/mod.rs"), "store");
        assert_eq!(file_module("pkg/sub/__init__.py"), "sub");
        assert_eq!(file_module("pkg/util.py"), "util");
        assert_eq!(file_module("web/app.js"), "app.js");
    }

    #[test]
    fn a_rust_method_belongs_to_its_impl_type_even_for_a_trait_or_a_closure() {
        let path = "src/store/graph_read.rs";
        let text = "impl<T> Display for a::Store<T> where T: Copy { fn open() {} }";
        let mut block = symbol(
            "impl",
            "<implementation@0>",
            SymbolKind::Class,
            Some("file"),
            0,
        );
        block.range.end_byte = text.len();
        let columns = DocumentColumns {
            language: "rust".into(),
            nodes: nodes(vec![
                symbol("file", path, SymbolKind::Module, None, 0),
                block,
                symbol("m", "open", SymbolKind::Method, Some("impl"), 11),
                symbol("c", "<closure>", SymbolKind::Function, Some("m"), 12),
            ]),
            text: text.into(),
        };
        for id in ["m", "c"] {
            assert_eq!(
                columns.owner(&columns.nodes[id]),
                Owner::RustType {
                    name: "Store".into(),
                    scope: path.into()
                }
            );
        }
    }

    #[test]
    fn a_rust_impl_of_a_type_that_has_no_name_falls_back_to_its_module() {
        let path = "src/store/graph_read.rs";
        let text = "impl Tr for [u8] { fn open() {} }";
        let mut block = symbol(
            "impl",
            "<implementation@0>",
            SymbolKind::Class,
            Some("file"),
            0,
        );
        block.range.end_byte = text.len();
        let columns = DocumentColumns {
            language: "rust".into(),
            nodes: nodes(vec![
                symbol("file", path, SymbolKind::Module, None, 0),
                block,
                symbol("m", "open", SymbolKind::Method, Some("impl"), 11),
            ]),
            text: text.into(),
        };
        let Owner::Column(column) = columns.owner(&columns.nodes["m"]) else {
            panic!("a module column")
        };
        assert_eq!(column.label, "graph_read");
    }

    #[test]
    fn a_rust_free_function_belongs_to_its_file_and_inline_modules() {
        let path = "src/store/graph_read.rs";
        let columns = DocumentColumns {
            language: "rust".into(),
            nodes: nodes(vec![
                symbol("file", path, SymbolKind::Module, None, 0),
                symbol("mod", "tests", SymbolKind::Module, Some("file"), 1),
                symbol("f", "helper", SymbolKind::Function, Some("mod"), 2),
                symbol("g", "top", SymbolKind::Function, Some("file"), 3),
            ]),
            text: String::new(),
        };
        let label = |id: &str| match columns.owner(&columns.nodes[id]) {
            Owner::Column(column) => column.label,
            other => panic!("{other:?}"),
        };
        assert_eq!(label("f"), "graph_read::tests");
        assert_eq!(label("g"), "graph_read");
    }

    #[test]
    fn a_class_is_its_own_column_and_an_anonymous_class_defers_outward() {
        let columns = DocumentColumns {
            language: "java".into(),
            nodes: nodes(vec![
                symbol(
                    "file",
                    "src/store/graph_read.rs",
                    SymbolKind::Module,
                    None,
                    0,
                ),
                symbol("outer", "Outer", SymbolKind::Class, Some("file"), 1),
                symbol("anon", "<class@2>", SymbolKind::Class, Some("outer"), 2),
                symbol("m", "run", SymbolKind::Method, Some("anon"), 3),
            ]),
            text: String::new(),
        };
        let outer = Owner::Column(SequenceColumn {
            id: "outer".into(),
            label: "Outer".into(),
        });
        assert_eq!(columns.owner(&columns.nodes["m"]), outer);
        assert_eq!(columns.owner(&columns.nodes["outer"]), outer);
    }

    #[test]
    fn a_rust_type_name_joins_files_only_while_it_names_one_type() {
        let owner = |scope: &str| Owner::RustType {
            name: "Store".into(),
            scope: scope.into(),
        };
        let column = |scope, count: usize| owner(scope).column(&mut |_| Ok(count)).unwrap();
        assert_eq!(column("a.rs", 1), column("b.rs", 1));
        assert_ne!(column("a.rs", 2), column("b.rs", 2));
        assert_eq!(column("a.rs", 2), column("a.rs", 2));
        assert_eq!(column("a.rs", 2).label, "Store");
    }

    fn participant(id: &str, column: &str, label: &str, path: &str) -> Participant {
        let mut declaration = symbol(id, id, SymbolKind::Method, None, 1);
        declaration.path = path.into();
        Participant {
            id: id.into(),
            label: id.into(),
            kind: "method".into(),
            identification: String::new(),
            declaration: Some(declaration),
            column: Some(SequenceColumn {
                id: column.into(),
                label: label.into(),
            }),
        }
    }

    #[test]
    fn the_column_cap_counts_columns_not_methods() {
        let mut participants: Vec<_> = (0..MAX_COLUMNS)
            .map(|i| participant(&format!("m{i}"), &format!("c{i}"), "x", "a.rs"))
            .collect();
        participants.push(participant("again", "c0", "x", "a.rs"));
        let known = participant("next", "c1", "x", "a.rs");
        let new = participant("next", "new", "x", "a.rs");
        assert!(fits(&participants, known.column.as_ref().unwrap()));
        assert!(!fits(&participants, new.column.as_ref().unwrap()));
        participants.truncate(MAX_COLUMNS - 1);
        assert!(fits(&participants, new.column.as_ref().unwrap()));
    }

    #[test]
    fn columns_that_read_the_same_are_told_apart_by_path() {
        let mut participants = vec![
            participant("a", "1", "index.js", "web/a/index.js"),
            participant("b", "1", "index.js", "web/a/index.js"),
            participant("c", "2", "index.js", "web/b/index.js"),
            participant("d", "3", "Store", "src/s.rs"),
        ];
        disambiguate(&mut participants);
        let labels: Vec<_> = participants
            .iter()
            .map(|p| p.column.as_ref().unwrap().label.as_str())
            .collect();
        assert_eq!(
            labels,
            [
                "index.js (web/a/index.js)",
                "index.js (web/a/index.js)",
                "index.js (web/b/index.js)",
                "Store"
            ]
        );
    }
}
