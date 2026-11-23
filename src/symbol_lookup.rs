//! Human lookup keys are separate from native syntax identity and name normalization.
use crate::{
    files_view::{FILE_INPUT_LIMIT, SYNTAX_HANG_BACKSTOP},
    native_evidence::{Declaration, Key},
    syntax_bounds::{DEPTH, PARSE_TICKS, VISITS, parse_work},
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
/// by the source size, the parse-work bound and the syntax bounds alone, so the same document
/// gives the same answer however busy the machine is (#1119, #1220). A document beyond them is
/// refused, typed `input_bounds_exceeded` with the bound it hit, on every run, so a retry cannot
/// help (owner ruling 2026-11-18, #1119 follow-up). The parse-work bound stops a pathological
/// parse before it ends (owner ruling 2026-11-19, #1220). The wall clock is only a hang backstop
/// ([`SYNTAX_HANG_BACKSTOP`]), refused `deadline_exceeded`.
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
    let file = &Source {
        path,
        language,
        text,
    };
    let limit = Limit {
        check: &mut check,
        polls: 0,
    };
    derive(file, declarations, limit).map_err(|refused| match refused {
        Refused::Bound(bound) => anyhow::anyhow!(bound),
        Refused::Stopped(error) => error,
    })
}

/// The same keys for a view-time build (references, rules, interfaces), which must give the
/// same answer for the same input however busy the machine is (#1115). No wall clock decides
/// it: the Rust impl reparse is bounded by the source size, the parse-work bound and the syntax
/// bounds alone, and `check` is the build's own abort and hang backstop.
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

// Each deterministic refusal names the bound it hit; the MCP message is the text after the code.
const OVER_SIZE: &str =
    "input_bounds_exceeded: stable lookup source is over the 2 MiB syntax input bound";
const OVER_DEPTH: &str =
    "input_bounds_exceeded: stable lookup Rust syntax is over the 128-level depth bound";
const OVER_VISITS: &str =
    "input_bounds_exceeded: stable lookup Rust syntax is over the 1,000,000-node visit bound";
const OVER_MACRO: &str = "input_bounds_exceeded: stable lookup found no Rust impl syntax at its measured range within the macro expansion byte budget";
const OVER_PARSE: &str =
    "input_bounds_exceeded: stable lookup Rust parse is over the 80,000-tick parse-work bound";
const UNPARSED: &str =
    "input_bounds_exceeded: stable lookup Rust parse stopped short of a syntax tree";
const MISSING_IMPL: &str = "input_bounds_exceeded: stable lookup Rust impl declaration is not in the measured declarations";
const HUNG: &str = "deadline_exceeded: stable lookup Rust syntax hang backstop reached";

/// What bounds one Rust impl reparse besides the input bounds: the caller's check, polled once
/// in [`POLL`] steps, the first step included.
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
    if file.text.len() > FILE_INPUT_LIMIT {
        return Err(Refused::Bound(OVER_SIZE));
    }
    let mut parser = crate::measured_parse::parser();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| Refused::Stopped(error.into()))?;
    // The parser reports only that it stopped, so the reason waits here. The parse-work
    // bound comes first: once it is reached, the clock can no longer change the answer. Error
    // recovery counts more per tick (#1227).
    let mut stopped = None;
    let mut ticks = 0;
    let mut progress = |state: &tree_sitter::ParseState| {
        ticks += parse_work(state);
        let step = if ticks > PARSE_TICKS {
            Err(Refused::Bound(OVER_PARSE))
        } else {
            limit.step()
        };
        match step {
            Ok(()) => std::ops::ControlFlow::Continue(()),
            Err(refused) => {
                stopped = Some(refused);
                std::ops::ControlFlow::Break(())
            }
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
        (None, None) => return Err(Refused::Bound(UNPARSED)),
    };
    let mut visits = 0;
    bounded_tree(tree.root_node(), &mut limit, &mut visits)?;
    // The measured byte budget and the rest of the parse-work bound bound the expansions; no
    // clock interrupts them.
    let mut expansions =
        crate::native_rust_macros::Expansions::new(file.text).with_parse_ticks(PARSE_TICKS - ticks);
    let mut checked = std::collections::BTreeSet::new();
    checked.insert(tree.root_node().id());
    let mut targets = BTreeMap::new();
    for (path, declaration) in required {
        limit.step()?;
        let range = &declaration.range;
        let root = expansions
            .root_within_ticks(tree.root_node(), range.start, range.end)
            .ok_or(Refused::Bound(OVER_PARSE))?;
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
            .ok_or(Refused::Bound(OVER_MACRO))?;
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
        if *visits > VISITS {
            return Err(Refused::Bound(OVER_VISITS));
        }
        if depth >= DEPTH {
            return Err(Refused::Bound(OVER_DEPTH));
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
        for _ in 0..2 {
            let error = keys(&file, &native.declarations).unwrap_err();
            assert_eq!(error.to_string(), OVER_SIZE);
        }
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

    /// The MCP derivation on a clock that never moves: however loaded the machine, the hang
    /// backstop cannot answer first, so only the input decides (#1220).
    fn keys_on_a_stopped_clock(
        file: &SourceFile,
        declarations: &[Declaration],
    ) -> Result<BTreeMap<String, String>> {
        let stopped = Instant::now();
        keys_until(
            &file.path,
            &file.language,
            &file.text,
            declarations,
            &move || stopped,
        )
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
        // Each `0,` is two syntax nodes, so this is just over the visit bound (#1220).
        let wide = format!("\nconst WIDE: [u8; 0] = [{}];", "0,".repeat(VISITS / 2 + 1));
        for (tail, bound) in [(deep, OVER_DEPTH), (wide, OVER_VISITS)] {
            let mut beyond = file.clone();
            beyond.text.push_str(&tail);
            assert!(beyond.text.len() <= FILE_INPUT_LIMIT);
            for _ in 0..2 {
                let error = keys_on_a_stopped_clock(&beyond, &native.declarations).unwrap_err();
                assert_eq!(error.to_string(), bound);
                let found = checked(&beyond, &native.declarations, &mut || Ok(())).unwrap();
                assert_eq!(found, None);
            }
        }
    }

    /// Owner ruling 2026-11-19 (#1220): parse work is a bound of its own, counted in
    /// tree-sitter progress ticks, never time. A 2 MiB array needs about 126,000 ticks to
    /// parse, so it is refused by the parse-work bound, not by the visit bound its whole parse
    /// would reach: its parse stops at 80,000. A large macro token tree is beyond it too though
    /// it is within the visit bound. On every run, on a clock that never moves, and the same
    /// for a checked derivation.
    #[test]
    fn a_document_beyond_the_parse_work_bound_is_refused_before_its_parse_ends() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let head = "\nconst WIDE: [u8; 0] = [";
        let fill = (FILE_INPUT_LIMIT - file.text.len() - head.len() - 2) / 2;
        let wide = format!("{head}{}];", "0,".repeat(fill));
        let tokens = format!("\nm! {{ {} }}", "0,".repeat(400_000));
        for tail in [wide, tokens] {
            let mut beyond = file.clone();
            beyond.text.push_str(&tail);
            assert!(beyond.text.len() <= FILE_INPUT_LIMIT);
            for _ in 0..2 {
                let error = keys_on_a_stopped_clock(&beyond, &native.declarations).unwrap_err();
                assert_eq!(error.to_string(), OVER_PARSE);
                let (code, retryable, message) = mcp_answer(&error);
                assert_eq!((code.as_str(), retryable), ("input_bounds_exceeded", false));
                assert!(message.contains("parse-work bound"), "{message}");
                let found = checked(&beyond, &native.declarations, &mut || Ok(())).unwrap();
                assert_eq!(found, None);
            }
        }
        let mut parser = crate::measured_parse::parser();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tokens = format!("{IMPL}\nm! {{ {} }}", "0,".repeat(400_000));
        let tree = parser.parse(&tokens, None).unwrap();
        assert!(tree.root_node().descendant_count() < VISITS);
    }

    /// `levels` nested `assert!` levels around a 100,000-element array, an impl innermost.
    /// Measurement expands them all, and the lookup re-parses each level on the way to the
    /// impl.
    fn nested_macros(levels: usize) -> String {
        let mut inner = format!("impl S {{ fn f() {{}} }} [{}]; true", "0,".repeat(100_000));
        for _ in 0..levels {
            inner = format!("assert!({{ {inner} }}); true");
        }
        format!("struct S;\nfn m() -> bool {{ {inner} }}\n")
    }

    /// The document's parse and its macro re-parses share the parse-work bound (#1220): each
    /// level re-parses the payload again (about 20,000 ticks), so four levels are refused,
    /// deterministically, while two levels of the same payload fit and the impl has its key.
    #[test]
    fn macro_reparses_share_the_parse_work_bound() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", &nested_macros(2));
        let found = keys_on_a_stopped_clock(&file, &native.declarations).unwrap();
        assert!(
            found.values().any(|key| key == "store.rs::m::S::f"),
            "{found:?}"
        );
        let (file, native) = fixture.measure("store.rs", &nested_macros(4));
        assert!(
            native
                .declarations
                .iter()
                .any(|d| d.name.as_deref() == Some("f")),
            "measurement expands every level"
        );
        for _ in 0..2 {
            let error = keys_on_a_stopped_clock(&file, &native.declarations).unwrap_err();
            assert_eq!(error.to_string(), OVER_PARSE);
            let found = checked(&file, &native.declarations, &mut || Ok(())).unwrap();
            assert_eq!(found, None);
        }
    }

    /// Ordinary documents near the other bounds stay within the parse-work bound (#1220): an
    /// array just under the visit bound, the densest parse work measured within it (about
    /// 59,000 ticks), and 2 MiB of impl blocks (about 19,000).
    #[test]
    fn documents_near_the_other_bounds_still_have_keys() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let array = format!("\nconst W: [u8; 0] = [{}];", "0,".repeat(VISITS / 2 - 100));
        let block = "\nimpl Saaaaaaaa { fn fbbbbbbbb() {} }";
        let impls = block.repeat((FILE_INPUT_LIMIT - file.text.len()) / block.len());
        for tail in [array, impls] {
            let mut near = file.clone();
            near.text.push_str(&tail);
            assert!(near.text.len() <= FILE_INPUT_LIMIT);
            let found = keys_on_a_stopped_clock(&near, &native.declarations).unwrap();
            assert!(found.values().any(|key| key == "store.rs::Store::save"));
        }
    }

    /// Owner ruling 2026-11-20 (#1227): a document that is mostly syntax errors is refused by
    /// the parse-work bound, its error recovery counting more per tick, on every run and on a
    /// clock that never moves, so the hang backstop has no part in it. Its ticks alone are
    /// under the bound. A clean document of the same size counts exactly its ticks and keeps
    /// its keys.
    #[test]
    fn a_document_of_syntax_errors_is_refused_by_the_parse_work_bound() {
        use crate::syntax_bounds::parse_work_inputs::{
            clean_of_size, syntax_errors, ticks_and_work,
        };
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let mut errors = file.clone();
        errors.text.push_str(&syntax_errors());
        let (ticks, work) = ticks_and_work("rust", &errors.text);
        assert!(ticks < PARSE_TICKS && work > PARSE_TICKS, "{ticks} {work}");
        for _ in 0..2 {
            let error = keys_on_a_stopped_clock(&errors, &native.declarations).unwrap_err();
            assert_eq!(error.to_string(), OVER_PARSE);
            let (code, retryable, _) = mcp_answer(&error);
            assert_eq!((code.as_str(), retryable), ("input_bounds_exceeded", false));
            let found = checked(&errors, &native.declarations, &mut || Ok(())).unwrap();
            assert_eq!(found, None);
        }
        let mut clean = file.clone();
        clean.text.push('\n');
        let tail = clean_of_size("rust", errors.text.len() - clean.text.len());
        clean.text.push_str(&tail);
        let (ticks, work) = ticks_and_work("rust", &clean.text);
        assert_eq!(work, ticks);
        let found = keys_on_a_stopped_clock(&clean, &native.declarations).unwrap();
        assert!(found.values().any(|key| key == "store.rs::Store::save"));
    }

    /// The MCP answer to an error of the derivation: its code, retryability and message.
    fn mcp_answer(error: &anyhow::Error) -> (String, bool, String) {
        let code = crate::mcp::mcp_read_error(error);
        let envelope = crate::mcp::tools::failure_for(&serde_json::json!(1), code, error);
        let error = &envelope["error"];
        (
            error["code"].as_str().unwrap().to_owned(),
            error["retryable"].as_bool().unwrap(),
            error["message"].as_str().unwrap().to_owned(),
        )
    }

    /// Owner ruling 2026-11-18 (#1119 follow-up): a document beyond an input bound answers
    /// the non-retryable `input_bounds_exceeded`, naming the bound, the same on every run;
    /// only the hang backstop answers the retryable `deadline_exceeded`.
    #[test]
    fn mcp_answers_a_bound_as_input_bounds_exceeded_and_the_backstop_as_a_deadline() {
        let fixture = Fixture::new();
        let (file, native) = fixture.measure("store.rs", IMPL);
        let mut oversized = file.clone();
        oversized.text.push_str(&" ".repeat(FILE_INPUT_LIMIT));
        let mut deep = file.clone();
        deep.text.push_str(&format!(
            "\nfn deep() {{ {}1{} }}",
            "(".repeat(DEPTH),
            ")".repeat(DEPTH)
        ));
        // The visit bound's own message is pinned above; its million-node walk is slow in a
        // debug build, so it is not walked again here.
        for (beyond, bound) in [
            (&oversized, "2 MiB syntax input bound"),
            (&deep, "128-level depth bound"),
        ] {
            let first = mcp_answer(&keys(beyond, &native.declarations).unwrap_err());
            let second = mcp_answer(&keys(beyond, &native.declarations).unwrap_err());
            assert_eq!(first, second);
            assert_eq!(first.0, "input_bounds_exceeded");
            assert!(!first.1, "a retry gets the same refusal");
            assert!(first.2.contains(bound), "{}", first.2);
        }
        let hung = clock_jumping_to(SYNTAX_HANG_BACKSTOP);
        let error = keys_until(
            &file.path,
            &file.language,
            &file.text,
            &native.declarations,
            &hung,
        )
        .unwrap_err();
        let (code, retryable, message) = mcp_answer(&error);
        assert_eq!(code, "deadline_exceeded");
        assert!(retryable);
        assert_eq!(message, "Deadline exceeded");
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
