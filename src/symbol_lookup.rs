//! Human lookup keys are separate from native syntax identity and name normalization.
use crate::{
    files_view::{FILE_INPUT_LIMIT, FILE_TIME_LIMIT},
    native_evidence::{Declaration, Key},
    syntax_bounds::{DEPTH, VISITS},
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, time::Instant};
use tree_sitter::Node;

/// Every named, recoverable declaration, keyed by its unchanged native SID. Values need not
/// be unique: overloads, repeated declarations and separate impls intentionally collide.
/// Anonymous containers other than Rust impls have no stable written name, so their contents
/// have no lookup key. An incomplete Rust parse refuses the document, never a partial map.
pub(crate) fn keys_from_source(
    path: &str,
    language: &str,
    text: &str,
    declarations: &[Declaration],
) -> Result<BTreeMap<String, String>> {
    let file = &Source {
        path,
        language,
        text,
    };
    let implementations: BTreeMap<_, _> = declarations
        .iter()
        .filter(|d| d.kind == "implementation" && d.name.is_none())
        .map(|d| {
            let mut path = d.ancestors.clone();
            path.push(d.key.clone());
            Ok((serde_json::to_vec(&path)?, d))
        })
        .collect::<Result<_>>()?;
    let mut required = BTreeMap::new();
    if file.language == "rust" {
        for d in declarations.iter().filter(|d| recoverable(file, d)) {
            for (i, ancestor) in d.ancestors.iter().enumerate() {
                if ancestor.kind == "implementation" && ancestor.name.is_none() {
                    let path = serde_json::to_vec(&d.ancestors[..=i])?;
                    let block = implementations.get(&path).context(
                        "deadline_exceeded: stable lookup missing Rust impl declaration",
                    )?;
                    required.insert(path, *block);
                }
            }
        }
    }
    let targets = if required.is_empty() {
        BTreeMap::new()
    } else {
        impl_targets(file, &required)?
    };
    let mut result = BTreeMap::new();
    for d in declarations.iter().filter(|d| recoverable(file, d)) {
        let mut parts = vec![file.path.to_owned()];
        let mut complete = true;
        for (i, ancestor) in d.ancestors.iter().enumerate() {
            if file_module(file, i, ancestor) {
                continue;
            }
            if let Some(name) = &ancestor.name {
                parts.push(name.clone());
            } else if let Some(Some(name)) = targets.get(&serde_json::to_vec(&d.ancestors[..=i])?) {
                parts.push(name.clone());
            } else {
                complete = false;
                break;
            }
        }
        if complete {
            parts.push(d.name.clone().expect("recoverable named declaration"));
            result.insert(d.syntax_id.clone(), parts.join("::"));
        }
    }
    Ok(result)
}

struct Source<'a> {
    path: &'a str,
    language: &'a str,
    text: &'a str,
}

#[cfg(test)]
fn keys(
    file: &crate::model::SourceFile,
    declarations: &[Declaration],
) -> Result<BTreeMap<String, String>> {
    keys_from_source(&file.path, &file.language, &file.text, declarations)
}

fn file_module(file: &Source<'_>, index: usize, key: &Key) -> bool {
    index == 0 && key.kind == "module" && key.name.as_deref().is_none_or(|name| name == file.path)
}

fn recoverable(file: &Source<'_>, d: &Declaration) -> bool {
    d.name.is_some()
        && d.ancestors.iter().enumerate().all(|(i, key)| {
            key.name.is_some()
                || file_module(file, i, key)
                || (file.language == "rust" && key.kind == "implementation")
        })
}

fn exhausted() -> anyhow::Error {
    anyhow::anyhow!("deadline_exceeded: stable lookup Rust syntax budget exhausted")
}

fn impl_targets(
    file: &Source<'_>,
    required: &BTreeMap<Vec<u8>, &Declaration>,
) -> Result<BTreeMap<Vec<u8>, Option<String>>> {
    ensure!(file.text.len() <= FILE_INPUT_LIMIT, exhausted());
    let deadline = Instant::now() + FILE_TIME_LIMIT;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&tree_sitter_rust::LANGUAGE.into())?;
    let mut progress = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let tree = parser
        .parse_with_options(
            &mut |offset, _| &file.text.as_bytes()[offset..],
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
        )
        .ok_or_else(exhausted)?;
    let mut visits = 0;
    bounded_tree(tree.root_node(), deadline, &mut visits)?;
    let mut expansions =
        crate::native_rust_macros::Expansions::new(file.text).with_deadline(deadline);
    let mut checked = std::collections::BTreeSet::new();
    checked.insert(tree.root_node().id());
    let mut targets = BTreeMap::new();
    for (path, declaration) in required {
        ensure!(Instant::now() < deadline, exhausted());
        let range = &declaration.range;
        let root = expansions.root_for(tree.root_node(), range.start, range.end);
        if checked.insert(root.id()) {
            bounded_tree(root, deadline, &mut visits)?;
        }
        // Expansion's absence can mean its byte budget ran out. Never treat that as proof
        // that another impl is the only match. All requested native impls need exact syntax.
        let node = root
            .descendant_for_byte_range(range.start, range.end)
            .filter(|node| {
                node.kind() == "impl_item"
                    && node.start_byte() == range.start
                    && node.end_byte() == range.end
            })
            .ok_or_else(exhausted)?;
        let target = node
            .child_by_field_name("type")
            .and_then(|node| written_type(node, file.text.as_bytes()));
        ensure!(Instant::now() < deadline, exhausted());
        targets.insert(path.clone(), target);
    }
    Ok(targets)
}

/// A bounded iterative walk also bounds the later tree-sitter range/path lookups. Expansion
/// trees share the document's visit budget rather than getting one budget per declaration.
fn bounded_tree(root: Node<'_>, deadline: Instant, visits: &mut usize) -> Result<()> {
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        *visits += 1;
        ensure!(
            *visits <= VISITS && depth < DEPTH && Instant::now() < deadline,
            exhausted()
        );
        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.node() == root {
                return Ok(());
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return Ok(());
            }
            depth -= 1;
        }
    }
}

/// Written self-type path, without generic arguments or reference qualifiers. This is not
/// type resolution: every trait impl and specialization of the same path shares a key.
fn written_type(node: Node<'_>, text: &[u8]) -> Option<String> {
    let mut pending = vec![(node, 0)];
    let mut parts = Vec::new();
    while let Some((node, depth)) = pending.pop() {
        if depth >= DEPTH {
            return None;
        }
        match node.kind() {
            "type_identifier" | "identifier" | "crate" | "self" | "super" => {
                parts.push(node.utf8_text(text).ok()?.to_owned());
            }
            "generic_type" | "reference_type" => {
                pending.push((node.child_by_field_name("type")?, depth + 1));
            }
            "scoped_type_identifier" | "scoped_identifier" => {
                pending.push((node.child_by_field_name("name")?, depth + 1));
                if let Some(path) = node.child_by_field_name("path") {
                    pending.push((path, depth + 1));
                } else {
                    // Preserve a leading `::`, which is part of the written path.
                    parts.push(String::new());
                }
            }
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        capture::Capture,
        indexer::IndexOptions,
        model::SourceFile,
        native_evidence::{Artifact, from_capture},
        store::Store,
    };
    use std::{
        fs,
        sync::{Arc, atomic::AtomicBool},
    };

    struct Fixture {
        _state: tempfile::TempDir,
        workspace: tempfile::TempDir,
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let state = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
            Self {
                _state: state,
                workspace,
                store,
            }
        }
        fn measure(&self, path: &str, text: &str) -> (SourceFile, Artifact) {
            let target = self.workspace.path().join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, text).unwrap();
            let cancel = Arc::new(AtomicBool::new(false));
            let capture = Capture::admit(
                &IndexOptions::new(self.workspace.path().to_owned()),
                &cancel,
                &|_| {},
            )
            .unwrap();
            let root = fs::canonicalize(self.workspace.path()).unwrap();
            let artifact = from_capture(&capture, &root, self.store.root_id(), &cancel).unwrap();
            let file = capture
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .clone();
            (file, artifact)
        }
    }

    #[test]
    fn rust_store_method_has_a_written_key_without_changing_native_facts() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure(
            "src/store.rs",
            "struct Store; impl Store { fn save(&self) {} }",
        );
        let before = native.declarations.clone();
        let result = keys(&file, &native.declarations).unwrap();
        let method = native
            .declarations
            .iter()
            .find(|d| d.name.as_deref() == Some("save"))
            .unwrap();
        assert_eq!(result[&method.syntax_id], "src/store.rs::Store::save");
        assert_eq!(method.lookup_key.as_deref(), Some("save"));
        assert_eq!(native.declarations, before);
        assert!(
            native
                .declarations
                .iter()
                .filter(|d| d.name.is_none())
                .all(|d| !result.contains_key(&d.syntax_id))
        );
    }

    #[test]
    fn distinct_impl_sids_keep_the_same_lookup_key() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure(
            "store.rs",
            "            struct Store; trait First { fn save(); } trait Second { fn save(); }
            impl First for Store { fn save() {} }
            impl Second for Store { fn save() {} }",
        );
        let methods: Vec<_> = native
            .declarations
            .iter()
            .filter(|d| {
                d.name.as_deref() == Some("save")
                    && d.ancestors.iter().any(|k| k.kind == "implementation")
            })
            .collect();
        assert_eq!(methods.len(), 2);
        assert_ne!(methods[0].syntax_id, methods[1].syntax_id);
        let result = keys(&file, &native.declarations).unwrap();
        assert_eq!(result[&methods[0].syntax_id], "store.rs::Store::save");
        assert_eq!(result[&methods[1].syntax_id], "store.rs::Store::save");
    }

    #[test]
    fn an_earlier_impl_changes_existing_sid_but_not_lookup_key() {
        let fixture = Fixture::new();
        let original = "struct Store; impl Store { fn save() {} }";
        let (file, native) = fixture.measure("store.rs", original);
        let method = native
            .declarations
            .iter()
            .find(|d| d.name.as_deref() == Some("save"))
            .unwrap();
        let sid = method.syntax_id.clone();
        let key = keys(&file, &native.declarations).unwrap()[&sid].clone();
        let (edited, native) = fixture.measure(
            "store.rs",
            &format!("struct Other; impl Other {{}}\n{original}"),
        );
        let method = native
            .declarations
            .iter()
            .find(|d| d.name.as_deref() == Some("save"))
            .unwrap();
        assert_ne!(method.syntax_id, sid);
        assert_eq!(
            keys(&edited, &native.declarations).unwrap()[&method.syntax_id],
            key
        );
    }

    #[test]
    fn named_language_ancestors_need_no_additional_parse() {
        for (path, text) in [
            ("A.java", "class A { void run() {} }"),
            ("a.py", "class A:\n    def run(self):\n        pass\n"),
            ("a.js", "class A { run() {} }"),
        ] {
            let fixture = Fixture::new();
            let (mut file, native) = fixture.measure(path, text);
            // Keys in these languages use measured names, not a second syntax parse.
            file.text.clear();
            let method = native
                .declarations
                .iter()
                .find(|d| d.name.as_deref() == Some("run"))
                .unwrap();
            assert_eq!(
                keys(&file, &native.declarations).unwrap()[&method.syntax_id],
                format!("{path}::A::run")
            );
        }
    }

    #[test]
    fn scoped_generic_self_types_keep_paths_but_not_arguments() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure(
            "store.rs",
            "            impl<T> First for crate::data::Store<T> { fn save() {} }
            impl Second for crate::data::Store<u8> { fn save() {} }",
        );
        let result = keys(&file, &native.declarations).unwrap();
        let methods: Vec<_> = native
            .declarations
            .iter()
            .filter(|d| d.name.as_deref() == Some("save"))
            .collect();
        assert_eq!(methods.len(), 2);
        for method in methods {
            assert_eq!(
                result[&method.syntax_id],
                "store.rs::crate::data::Store::save"
            );
        }
    }

    #[test]
    fn impls_in_supported_macros_use_their_exact_expanded_ranges() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", "            struct Store; fn local() { assert!({ impl Store { fn save() {} } true }); }");
        let result = keys(&file, &native.declarations).unwrap();
        let method = native
            .declarations
            .iter()
            .find(|d| d.name.as_deref() == Some("save"))
            .unwrap();
        assert_eq!(result[&method.syntax_id], "store.rs::local::Store::save");
    }

    #[test]
    fn an_exhausted_impl_parse_refuses_the_whole_map() {
        let fixture = Fixture::new();
        let (mut file, native) =
            fixture.measure("store.rs", "struct Store; impl Store { fn save() {} }");
        file.text.push_str(&" ".repeat(FILE_INPUT_LIMIT));
        let error = keys(&file, &native.declarations).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("deadline_exceeded: stable lookup")
        );
    }
}
