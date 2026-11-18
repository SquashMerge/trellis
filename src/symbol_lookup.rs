//! Human lookup keys are separate from native syntax identity and name normalization.
use crate::{
    files_view::{FILE_INPUT_LIMIT, SYNTAX_HANG_BACKSTOP},
    native_evidence::{Declaration, Key},
    syntax_bounds::{DEPTH, VISITS},
};
use anyhow::Result;
use std::{collections::BTreeMap, time::Instant};
use tree_sitter::Node;

/// Every named, recoverable declaration, keyed by its unchanged native SID. Values need not
/// be unique: overloads, repeated declarations and separate impls intentionally collide.
/// Anonymous containers other than Rust impls have no stable written name, so their contents
/// have no lookup key. An incomplete Rust parse refuses the document, never a partial map.
///
/// This is the MCP read's derivation. Like [`keys_checked`], the Rust impl reparse is bounded
/// by the source size and the syntax bounds alone, so the same document gives the same answer
/// however busy the machine is (#1119). A document beyond them is refused, typed
/// `deadline_exceeded`, on every run. The wall clock is only a hang backstop
/// ([`SYNTAX_HANG_BACKSTOP`]), refused with the same code.
pub(crate) fn keys_from_source(
    path: &str,
    language: &str,
    text: &str,
    declarations: &[Declaration],
) -> Result<BTreeMap<String, String>> {
    keys_until(path, language, text, declarations, &Instant::now)
}

/// [`keys_from_source`] on the clock `now`.
fn keys_until(
    path: &str,
    language: &str,
    text: &str,
    declarations: &[Declaration],
    now: &dyn Fn() -> Instant,
) -> Result<BTreeMap<String, String>> {
    let backstop = now() + SYNTAX_HANG_BACKSTOP;
    let mut check = || {
        anyhow::ensure!(now() < backstop, HUNG);
        Ok(())
    };
    keys_checked(path, language, text, declarations, &mut check)?
        .ok_or_else(|| anyhow::anyhow!(EXHAUSTED))
}

/// The same keys for a view-time build (references, rules, interfaces), which must give the
/// same answer for the same input however busy the machine is (#1115). No wall clock decides
/// it: the Rust impl reparse is bounded by the source size and the syntax bounds alone, and
/// `check` is the build's own abort and hang backstop.
///
/// `Ok(None)` is a document beyond those bounds. It has no stable keys on any run, so a
/// caller may keep that. An `Err` is `check`'s own error, and never an answer to keep.
pub(crate) fn keys_checked(
    path: &str,
    language: &str,
    text: &str,
    declarations: &[Declaration],
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<Option<BTreeMap<String, String>>> {
    let file = &Source {
        path,
        language,
        text,
    };
    match derive(file, declarations, Limit { check, polls: 0 }) {
        Ok(keys) => Ok(Some(keys)),
        Err(Refused::Bound(_)) => Ok(None),
        Err(Refused::Stopped(error)) => Err(error),
    }
}

/// The keys of a document in a language without Rust impls: every container is named or
/// keyless, so there is nothing to reparse and nothing that can fail.
pub(crate) fn keys_by_name(
    path: &str,
    language: &str,
    declarations: &[Declaration],
) -> BTreeMap<String, String> {
    debug_assert_ne!(language, "rust", "Rust impl containers need their source");
    let file = &Source {
        path,
        language,
        text: "",
    };
    assemble(file, declarations, &BTreeMap::new())
}

const EXHAUSTED: &str = "deadline_exceeded: stable lookup Rust syntax budget exhausted";
const MISSING_IMPL: &str = "deadline_exceeded: stable lookup missing Rust impl declaration";
const HUNG: &str = "deadline_exceeded: stable lookup Rust syntax hang backstop reached";

/// What bounds one Rust impl reparse besides the source size and the syntax bounds: the
/// caller's check, polled once in [`POLL`] steps, the first step included.
struct Limit<'a> {
    check: &'a mut dyn FnMut() -> Result<()>,
    polls: usize,
}
const POLL: usize = 256;
impl Limit<'_> {
    fn step(&mut self) -> Result<(), Refused> {
        let due = self.polls == 0;
        self.polls = (self.polls + 1) % POLL;
        if due {
            (self.check)().map_err(Refused::Stopped)?;
        }
        Ok(())
    }
}

/// Why a document has no key map.
enum Refused {
    /// A bound was reached. Every bound is a property of the input.
    Bound(&'static str),
    /// The caller's check stopped the reparse, or the parser could not start.
    Stopped(anyhow::Error),
}

fn derive(
    file: &Source<'_>,
    declarations: &[Declaration],
    limit: Limit<'_>,
) -> Result<BTreeMap<String, String>, Refused> {
    let implementations: BTreeMap<_, _> = declarations
        .iter()
        .filter(|d| d.kind == "implementation" && d.name.is_none())
        .map(|d| {
            let mut path = d.ancestors.clone();
            path.push(d.key.clone());
            (path_key(&path), d)
        })
        .collect();
    let mut required = BTreeMap::new();
    if file.language == "rust" {
        for d in declarations.iter().filter(|d| recoverable(file, d)) {
            for (i, ancestor) in d.ancestors.iter().enumerate() {
                if ancestor.kind == "implementation" && ancestor.name.is_none() {
                    let path = path_key(&d.ancestors[..=i]);
                    let block = implementations
                        .get(&path)
                        .ok_or(Refused::Bound(MISSING_IMPL))?;
                    required.insert(path, *block);
                }
            }
        }
    }
    let targets = if required.is_empty() {
        BTreeMap::new()
    } else {
        impl_targets(file, &required, limit)?
    };
    Ok(assemble(file, declarations, &targets))
}

/// The map key of a declaration path. These records are plain strings and numbers.
fn path_key(path: &[Key]) -> Vec<u8> {
    serde_json::to_vec(path).expect("native key serialization")
}

fn assemble(
    file: &Source<'_>,
    declarations: &[Declaration],
    targets: &BTreeMap<Vec<u8>, Option<String>>,
) -> BTreeMap<String, String> {
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
            } else if let Some(Some(name)) = targets.get(&path_key(&d.ancestors[..=i])) {
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
    result
}

struct Source<'a> {
    path: &'a str,
    language: &'a str,
    text: &'a str,
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

fn impl_targets(
    file: &Source<'_>,
    required: &BTreeMap<Vec<u8>, &Declaration>,
    mut limit: Limit<'_>,
) -> Result<BTreeMap<Vec<u8>, Option<String>>, Refused> {
    let exhausted = || Refused::Bound(EXHAUSTED);
    if file.text.len() > FILE_INPUT_LIMIT {
        return Err(exhausted());
    }
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| Refused::Stopped(error.into()))?;
    // The parser reports only that it stopped, so the reason waits here.
    let mut stopped = None;
    let mut progress = |_: &tree_sitter::ParseState| match limit.step() {
        Ok(()) => std::ops::ControlFlow::Continue(()),
        Err(refused) => {
            stopped = Some(refused);
            std::ops::ControlFlow::Break(())
        }
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &file.text.as_bytes()[offset..],
        None,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    let tree = match (tree, stopped) {
        (Some(tree), None) => tree,
        (_, Some(refused)) => return Err(refused),
        (None, None) => return Err(exhausted()),
    };
    let mut visits = 0;
    bounded_tree(tree.root_node(), &mut limit, &mut visits)?;
    // The measured byte budget bounds the expansions; no clock interrupts them.
    let mut expansions = crate::native_rust_macros::Expansions::new(file.text);
    let mut checked = std::collections::BTreeSet::new();
    checked.insert(tree.root_node().id());
    let mut targets = BTreeMap::new();
    for (path, declaration) in required {
        limit.step()?;
        let range = &declaration.range;
        let root = expansions.root_for(tree.root_node(), range.start, range.end);
        if checked.insert(root.id()) {
            bounded_tree(root, &mut limit, &mut visits)?;
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
        limit.step()?;
        targets.insert(path.clone(), target);
    }
    Ok(targets)
}

/// A bounded iterative walk also bounds the later tree-sitter range/path lookups. Expansion
/// trees share the document's visit budget rather than getting one budget per declaration.
fn bounded_tree(root: Node<'_>, limit: &mut Limit<'_>, visits: &mut usize) -> Result<(), Refused> {
    let mut cursor = root.walk();
    let mut depth = 0;
    loop {
        *visits += 1;
        if *visits > VISITS || depth >= DEPTH {
            return Err(Refused::Bound(EXHAUSTED));
        }
        limit.step()?;
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

    fn keys(file: &SourceFile, declarations: &[Declaration]) -> Result<BTreeMap<String, String>> {
        keys_from_source(&file.path, &file.language, &file.text, declarations)
    }

    fn checked(
        file: &SourceFile,
        declarations: &[Declaration],
        check: &mut dyn FnMut() -> Result<()>,
    ) -> Result<Option<BTreeMap<String, String>>> {
        keys_checked(&file.path, &file.language, &file.text, declarations, check)
    }

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

    const IMPL: &str = "struct Store; impl Store { fn save() {} }";

    /// A clock that reads `base` once, then `later` on every read after.
    fn clock_jumping_to(later: std::time::Duration) -> impl Fn() -> Instant {
        let base = Instant::now();
        let reads = std::cell::Cell::new(0);
        move || {
            reads.set(reads.get() + 1);
            if reads.get() == 1 { base } else { base + later }
        }
    }

    /// #1119: the MCP derivation no longer has a 250 ms budget. A clock that reads a second
    /// later on every poll, past the old Files budget, changes nothing.
    #[test]
    fn a_slow_machine_gets_the_same_mcp_keys() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let slow = clock_jumping_to(crate::files_view::FILE_TIME_LIMIT * 4);
        let found = keys_until(
            &file.path,
            &file.language,
            &file.text,
            &native.declarations,
            &slow,
        )
        .unwrap();
        assert_eq!(found, keys(&file, &native.declarations).unwrap());
        assert!(found.values().any(|key| key == "store.rs::Store::save"));
    }

    #[test]
    fn the_hang_backstop_is_a_typed_refusal_of_the_mcp_derivation() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let hung = clock_jumping_to(SYNTAX_HANG_BACKSTOP);
        let error = keys_until(
            &file.path,
            &file.language,
            &file.text,
            &native.declarations,
            &hung,
        )
        .unwrap_err();
        assert_eq!(error.to_string(), HUNG);
        assert!(HUNG.starts_with("deadline_exceeded:"));
    }

    /// Each syntax bound is a property of the document: the MCP derivation refuses it typed,
    /// a checked one has no keys, on every run and on any clock.
    #[test]
    fn a_document_beyond_the_depth_or_visit_bound_is_refused_on_every_run() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let deep = format!(
            "\nfn deep() {{ {}1{} }}",
            "(".repeat(DEPTH),
            ")".repeat(DEPTH)
        );
        let wide = format!("\nconst WIDE: [u8; 0] = [{}];", "0,".repeat(VISITS));
        for tail in [deep, wide] {
            let mut beyond = file.clone();
            beyond.text.push_str(&tail);
            assert!(beyond.text.len() <= FILE_INPUT_LIMIT);
            for _ in 0..2 {
                let error = keys(&beyond, &native.declarations).unwrap_err();
                assert_eq!(error.to_string(), EXHAUSTED);
                let found = checked(&beyond, &native.declarations, &mut || Ok(())).unwrap();
                assert_eq!(found, None);
            }
        }
    }

    /// #1115: a view-time derivation never reads the clock, so its answer cannot depend on
    /// how long the reparse took.
    #[test]
    fn a_checked_derivation_is_bounded_by_its_check_alone() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let mut polls = 0;
        let found = checked(&file, &native.declarations, &mut || {
            polls += 1;
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert!(polls > 0);
        assert_eq!(found, keys(&file, &native.declarations).unwrap());
    }

    #[test]
    fn a_failing_check_is_the_error_of_a_checked_derivation_never_an_empty_map() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let error = checked(&file, &native.declarations, &mut || {
            anyhow::bail!("request_cancelled: test")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "request_cancelled: test");
    }

    #[test]
    fn a_document_beyond_the_syntax_bounds_has_no_checked_keys_on_any_run() {
        let fixture = Fixture::new();
        let (mut file, native) = fixture.measure("store.rs", IMPL);
        file.text.push_str(&" ".repeat(FILE_INPUT_LIMIT));
        for _ in 0..2 {
            let found = checked(&file, &native.declarations, &mut || Ok(())).unwrap();
            assert_eq!(found, None);
        }
    }

    #[test]
    fn a_language_without_impls_needs_no_source_for_its_keys() {
        let fixture = Fixture::new();
        let (file, native) =
            fixture.measure("store.py", "class Store:\n    def save(self): pass\n");
        let by_name = keys_by_name(&file.path, &file.language, &native.declarations);
        assert_eq!(by_name, keys(&file, &native.declarations).unwrap());
        assert!(by_name.values().any(|key| key == "store.py::Store::save"));
    }
}
